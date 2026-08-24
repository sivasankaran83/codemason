//! Extraction of a repository's *contract surface*: the public declarations a
//! job must know before it starts, emitted whole.
//!
//! ## Why this exists
//!
//! A supervisor pastes this into a coding job's task text so the job does not
//! spend its budget rediscovering the API it is about to call. That was
//! previously done with a hand-rolled regex which matched a declaration and
//! stopped at the newline, so a multi-line declaration arrived as
//!
//! ```text
//! public sealed record LoopLimits(
//! public enum LoopOutcome
//! ```
//!
//! — the parameters and the members gone. A job handed that did not go and
//! read the files, because as far as it could tell it already knew the type.
//! It invented the missing members instead: 54 compile errors, three failed
//! fix cycles, about $0.14 burned. The same task rewritten with the full text
//! finished in six iterations for $0.013.
//!
//! So the rule this module is built around: **a truncated contract is worse
//! than no contract at all.** With no contract a job reads the source; with
//! half a contract it believes it is already informed and fabricates the
//! rest. Every decision below resolves in favour of completeness — a data
//! type is emitted with its whole body however long it is, a signature the
//! engine cannot complete falls back to the symbol's entire source span, and
//! the only text ever left out is an implementation body, which is announced
//! on the line where it was dropped and counted in the summary.
//!
//! ## Scope
//!
//! This backs `codemason index --surface`, an operator-facing subcommand
//! flag. It is not a model-facing tool and the tool cap in SPEC.md, which
//! governs what the model is offered, does not apply to it.
//!
//! Parsing is the engine's job, not this module's: symbols and their lines
//! come from the dependency graph `Index::build` already produced, and
//! signatures from `engine::outline`. What is added here is span resolution
//! and the completeness guarantee.

use std::collections::HashSet;

use serde::Serialize;

use crate::engine::DependencyGraph;
use crate::engine::graph::{FileNode, Symbol};
use crate::engine::outline::{extract_signature, is_well_formed};
use crate::text::normalize_slashes;

/// Hard ceiling on any one symbol's span. Only pathological input reaches it
/// — an unbalanced brace inside a string form the scanner misreads, say —
/// and reaching it is reported rather than absorbed.
const MAX_SPAN_LINES: usize = 400;

/// A container (class, trait, `impl`) longer than this has its member
/// declarations kept and its implementation bodies dropped. Data types are
/// never subject to this: their body *is* the contract.
const MAX_VERBATIM_LINES: usize = 60;

/// How far a signature may run before the joiner gives up and the caller
/// falls back to the full span.
const MAX_SIGNATURE_LINES: usize = 24;

/// Kinds whose members are the thing a caller needs. Emitted whole, always.
const DATA_KINDS: [&str; 4] = ["enum", "struct", "record", "type"];

/// Kinds that hold other declarations. Emitted whole when short enough,
/// otherwise as member declarations with bodies dropped.
const CONTAINER_KINDS: [&str; 9] = [
    "class",
    "interface",
    "trait",
    "impl",
    "object",
    "protocol",
    "extension",
    "namespace",
    "module",
];

/// Declaration modifiers, in every language the engine parses. Used to read a
/// declaration's visibility off its own line without a second parse.
const MODIFIERS: [&str; 26] = [
    "pub",
    "public",
    "private",
    "protected",
    "internal",
    "export",
    "default",
    "declare",
    "static",
    "abstract",
    "sealed",
    "virtual",
    "override",
    "async",
    "final",
    "partial",
    "readonly",
    "const",
    "extern",
    "unsafe",
    "open",
    "lateinit",
    "inline",
    "suspend",
    "mutating",
    "new",
];

#[derive(Debug, Serialize)]
pub struct SymbolSurface {
    pub name: String,
    pub kind: String,
    pub line: usize,
    /// `"signature"` — a complete declaration, body omitted because a
    /// function's body is not part of its contract.
    /// `"span"` — the symbol's source verbatim, members and all.
    /// `"members"` — member declarations kept, implementation bodies
    /// dropped. Only this form can carry `shortened`.
    pub form: &'static str,
    pub text: String,
    /// True when text was left out. Never true silently: the same fact is
    /// marked in `text` on the line it happened and counted in
    /// `SurfaceStats::shortened`.
    pub shortened: bool,
}

#[derive(Debug, Serialize)]
pub struct FileSurface {
    pub path: String,
    pub language: String,
    pub symbols: Vec<SymbolSurface>,
}

#[derive(Debug, Serialize)]
pub struct SurfaceStats {
    pub files: usize,
    pub symbols: usize,
    /// How many symbols had anything left out. A supervisor that wants a
    /// guaranteed-whole surface asserts this is zero.
    pub shortened: usize,
}

#[derive(Debug, Serialize)]
pub struct Surface {
    pub repo: String,
    /// The subdirectory asked for, when one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub files: Vec<FileSurface>,
    pub stats: SurfaceStats,
}

/// Extract the public surface of `graph`, optionally restricted to the
/// subdirectory `path` (forward- or back-slashed, repository-relative).
///
/// `repo` is a label for the report only; nothing is read from disk here,
/// because the graph already carries every file's source.
pub fn extract(graph: &DependencyGraph, repo: &str, path: Option<&str>) -> Surface {
    let prefix = path.map(prefix_key);

    let mut files = Vec::new();
    for key in graph.all_files() {
        let display = normalize_slashes(&key);
        if let Some(prefix) = &prefix
            && !is_under(&display, prefix)
        {
            continue;
        }
        let Some(node) = graph.deps(&key) else {
            continue;
        };
        files.push(FileSurface {
            path: display,
            language: node.language.clone(),
            symbols: symbols_for(node),
        });
    }

    let symbols = files.iter().map(|f| f.symbols.len()).sum();
    let shortened = files
        .iter()
        .flat_map(|f| f.symbols.iter())
        .filter(|s| s.shortened)
        .count();

    Surface {
        repo: normalize_slashes(repo),
        path: path.map(normalize_slashes),
        stats: SurfaceStats {
            files: files.len(),
            symbols,
            shortened,
        },
        files,
    }
}

/// Render the surface as text meant to be pasted straight into a task
/// string. Deliberately plain: no table drawing, no colour, nothing a
/// prompt would have to spend attention parsing.
pub fn render(surface: &Surface) -> String {
    let mut out = String::new();

    let scope = match &surface.path {
        Some(path) => format!("{} ({path})", surface.repo),
        None => surface.repo.clone(),
    };
    out.push_str(&format!("# contract surface: {scope}\n"));
    out.push_str(&format!(
        "# {} file(s), {} public symbol(s)\n",
        surface.stats.files, surface.stats.symbols
    ));
    out.push_str(
        "# Declarations are complete. Where anything was left out the line says so.\n",
    );

    for file in &surface.files {
        out.push_str(&format!("\n## {}\n", file.path));
        if file.symbols.is_empty() {
            out.push_str("(no public symbols recognised)\n");
            continue;
        }
        for symbol in &file.symbols {
            out.push_str(&format!(
                "\n{} {} (line {})\n",
                symbol.kind, symbol.name, symbol.line
            ));
            for line in symbol.text.lines() {
                if line.is_empty() {
                    out.push('\n');
                } else {
                    out.push_str(&format!("  {line}\n"));
                }
            }
        }
    }

    if surface.stats.shortened > 0 {
        out.push_str(&format!(
            "\n# {} symbol(s) above had an implementation body left out, marked in place.\n\
             # No declaration was shortened. Read the file before relying on anything\n\
             # a marker replaced.\n",
            surface.stats.shortened
        ));
    }

    out
}

fn symbols_for(node: &FileNode) -> Vec<SymbolSurface> {
    let lines: Vec<&str> = node.source.lines().collect();

    let mut ordered: Vec<&Symbol> = node.symbols.iter().collect();
    ordered.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));

    let mut seen: HashSet<(usize, &str, &str)> = HashSet::new();
    let mut out: Vec<SymbolSurface> = Vec::new();

    // The last line already covered by a symbol emitted verbatim. C# reports
    // a class and each of its methods as separate symbols, so without this
    // every member would appear twice — once inside its class and once on its
    // own. Only a *whole* span suppresses: if a container had anything left
    // out, its members are emitted separately instead, because suppressing
    // them against an incomplete rendering is exactly the truncation this
    // module exists to prevent.
    let mut covered_to = 0usize;

    for symbol in ordered {
        if !seen.insert((symbol.line, symbol.kind.as_str(), symbol.name.as_str())) {
            continue;
        }
        if symbol.line <= covered_to {
            continue;
        }
        let Some(index) = symbol.line.checked_sub(1) else {
            continue;
        };
        let Some(declaration) = lines.get(index) else {
            continue;
        };

        if !is_public(node, symbol, declaration) {
            // A private container hides its members too, so skip its whole
            // span rather than surfacing them one by one.
            if is_data_kind(&symbol.kind) || is_container_kind(&symbol.kind) {
                let span = span_of(&lines, index, &node.language);
                covered_to = span.end + 1;
            }
            continue;
        }

        let rendered = render_symbol(&lines, index, node, &symbol.kind, &symbol.name);
        if rendered.whole {
            covered_to = rendered.end + 1;
        }
        out.push(SymbolSurface {
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            line: symbol.line,
            form: rendered.form,
            text: rendered.text,
            shortened: rendered.shortened,
        });
    }

    out
}

struct Rendered {
    form: &'static str,
    text: String,
    shortened: bool,
    /// Last source line index this rendering accounts for.
    end: usize,
    /// True when every line between the declaration and `end` is present in
    /// `text`, so nested symbols need not be emitted again.
    whole: bool,
}

fn render_symbol(
    lines: &[&str],
    index: usize,
    node: &FileNode,
    kind: &str,
    name: &str,
) -> Rendered {
    let data = is_data_kind(kind);
    let container = is_container_kind(kind);

    if !data && !container {
        if let Some(signature) = signature_at(lines, index, name) {
            return Rendered {
                form: "signature",
                text: signature,
                shortened: false,
                end: index,
                whole: false,
            };
        }
        // The declaration could not be completed, so the span is the only
        // honest answer. Bulkier, and correct.
    }

    let span = span_of(lines, index, &node.language);
    let length = span.end.saturating_sub(index) + 1;

    if span.capped {
        // Only reachable on input the scanner could not make sense of. Say
        // so rather than presenting a clipped body as if it were the whole
        // thing.
        let mut text = dedent(&lines[index..=span.end]);
        text.push_str(&format!(
            "\n{} INCOMPLETE: stopped after {MAX_SPAN_LINES} lines. Read this file from \
             line {} before relying on {name}.",
            comment_prefix(&node.language),
            index + 1
        ));
        return Rendered {
            form: "span",
            text,
            shortened: true,
            end: span.end,
            whole: false,
        };
    }

    if data || length <= MAX_VERBATIM_LINES {
        return Rendered {
            form: "span",
            text: dedent(&lines[index..=span.end]),
            shortened: false,
            end: span.end,
            whole: true,
        };
    }

    let (text, dropped) = members(lines, index, span.end, &node.language);
    Rendered {
        form: "members",
        text,
        shortened: dropped,
        end: span.end,
        whole: false,
    }
}

/// A complete declaration for a symbol that has no members worth showing.
///
/// The engine's `extract_signature` is asked first and taken only when it
/// produced something well-formed *for this symbol* — it scans forward for
/// the first line its keyword regex recognises, which on a language whose
/// declarations do not start with such a keyword can be some later
/// definition entirely. Requiring the symbol's own name in the result is
/// what rules that out.
///
/// The fallback joiner below duplicates a little of what the engine's own
/// (private) joiner does. It is here rather than there because
/// `src/engine/` is vendored and must not be edited, and because a C# or
/// Java method — `public async Task<int> Fetch(` — matches none of the
/// engine's keywords and would otherwise come back as its first line alone,
/// which is the truncation this module exists to prevent.
fn signature_at(lines: &[&str], index: usize, name: &str) -> Option<String> {
    let window_end = (index + MAX_SIGNATURE_LINES).min(lines.len());
    let window = lines[index..window_end].join("\n");

    if let Some(signature) = extract_signature(&window)
        && signature.contains(name)
        && is_well_formed(&signature)
    {
        return Some(signature);
    }

    let joined = join_declaration(lines, index)?;
    if joined.contains(name) && is_well_formed(&joined) {
        Some(joined)
    } else {
        None
    }
}

/// Join a declaration across lines until it is syntactically finished,
/// stopping before the body. Returns `None` if it does not finish within
/// `MAX_SIGNATURE_LINES`, which sends the caller to the full span.
fn join_declaration(lines: &[&str], start: usize) -> Option<String> {
    let mut out = String::new();
    let mut depth: i32 = 0;

    for (offset, line) in lines.iter().enumerate().skip(start) {
        if offset - start >= MAX_SIGNATURE_LINES {
            return None;
        }
        let trimmed = line.trim();
        let mut piece = String::new();
        let mut terminated = false;

        for ch in trimmed.chars() {
            match ch {
                '{' | ';' if depth <= 0 => {
                    terminated = true;
                    break;
                }
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                _ => {}
            }
            piece.push(ch);
        }

        let piece = piece.trim_end();
        if !piece.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(piece);
        }

        if terminated {
            break;
        }
        // A line that closes its parentheses and does not obviously continue
        // is finished, even without a terminator — Kotlin and Swift often
        // have no `{` or `;` to stop on.
        if depth <= 0 && !ends_with_continuation(piece) {
            break;
        }
    }

    let out = out.trim().to_string();
    if out.is_empty() { None } else { Some(out) }
}

fn ends_with_continuation(line: &str) -> bool {
    matches!(
        line.chars().last(),
        Some(',') | Some('(') | Some('[') | Some('=') | Some('+') | Some('&') | Some('|')
            | Some('.') | Some('<') | Some('-')
    )
}

struct Span {
    /// Last line index belonging to the symbol.
    end: usize,
    /// True when `MAX_SPAN_LINES` stopped the scan rather than the source.
    capped: bool,
}

fn span_of(lines: &[&str], start: usize, language: &str) -> Span {
    if is_indentation_scoped(language) {
        indented_span(lines, start)
    } else {
        braced_span(lines, start)
    }
}

/// Brace-delimited languages. Also stops on a `;` reached before any body
/// opens, which is how a C# positional record, a Rust unit struct and an
/// abstract method all end.
fn braced_span(lines: &[&str], start: usize) -> Span {
    let mut scan = Scan::default();

    for (offset, line) in lines.iter().enumerate().skip(start) {
        if offset - start >= MAX_SPAN_LINES {
            return Span {
                end: offset.saturating_sub(1),
                capped: true,
            };
        }
        scan.line(line);
        if scan.finished {
            return Span {
                end: offset,
                capped: false,
            };
        }
    }

    Span {
        end: lines.len().saturating_sub(1),
        capped: false,
    }
}

/// Python and Ruby, where a body is delimited by indentation rather than
/// braces. A trailing blank line is not part of the symbol.
fn indented_span(lines: &[&str], start: usize) -> Span {
    let base = indent_width(lines[start]);
    let mut end = start;

    for (offset, line) in lines.iter().enumerate().skip(start + 1) {
        if offset - start >= MAX_SPAN_LINES {
            return Span { end, capped: true };
        }
        if line.trim().is_empty() {
            continue;
        }
        if indent_width(line) <= base {
            break;
        }
        end = offset;
    }

    Span { end, capped: false }
}

/// Brace/comment/string state carried across the lines of one span.
#[derive(Default)]
struct Scan {
    depth: i32,
    in_block_comment: bool,
    opened: bool,
    finished: bool,
}

impl Scan {
    fn line(&mut self, line: &str) {
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let ch = chars[i];

            if self.in_block_comment {
                if ch == '*' && chars.get(i + 1) == Some(&'/') {
                    self.in_block_comment = false;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }

            match ch {
                // A line comment, or a preprocessor directive whose braces
                // are not part of the construct being measured either way.
                '/' if chars.get(i + 1) == Some(&'/') => return,
                '#' => return,
                '/' if chars.get(i + 1) == Some(&'*') => {
                    self.in_block_comment = true;
                    i += 2;
                    continue;
                }
                '"' | '`' => {
                    i = skip_string(&chars, i, ch);
                    continue;
                }
                '\'' => {
                    // A quote that is not a character literal is a Rust
                    // lifetime, which is ordinary punctuation here.
                    if let Some(next) = char_literal_end(&chars, i) {
                        i = next;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                '{' => {
                    self.depth += 1;
                    self.opened = true;
                }
                '}' => {
                    self.depth -= 1;
                    if self.opened && self.depth <= 0 {
                        self.finished = true;
                        return;
                    }
                }
                ';' if !self.opened && self.depth <= 0 => {
                    self.finished = true;
                    return;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Index just past the closing delimiter, or the end of the line when the
/// literal does not close on it.
fn skip_string(chars: &[char], open: usize, delimiter: char) -> usize {
    let mut i = open + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            c if c == delimiter => return i + 1,
            _ => i += 1,
        }
    }
    chars.len()
}

/// Index just past a character literal starting at `open`, or `None` when
/// what starts there is not one.
fn char_literal_end(chars: &[char], open: usize) -> Option<usize> {
    if chars.get(open + 1) == Some(&'\\') {
        // An escape, so look a little further for the closing quote.
        for i in open + 2..(open + 12).min(chars.len()) {
            if chars[i] == '\'' {
                return Some(i + 1);
            }
        }
        return None;
    }
    if chars.get(open + 2) == Some(&'\'') {
        return Some(open + 3);
    }
    None
}

/// Member declarations with implementation bodies replaced by a marker.
///
/// Only ever applied to containers, and only past `MAX_VERBATIM_LINES`. A
/// member's own declaration always survives: what is dropped is strictly
/// what sits below it.
fn members(lines: &[&str], start: usize, end: usize, language: &str) -> (String, bool) {
    let kept = if is_indentation_scoped(language) {
        indented_member_mask(lines, start, end)
    } else {
        braced_member_mask(lines, start, end)
    };

    let block: Vec<&str> = lines[start..=end].to_vec();
    let pad = leading_whitespace(block[0]);
    let marker = comment_prefix(language);

    let mut out = String::new();
    let mut dropped_run = 0usize;
    let mut dropped_any = false;

    let flush = |out: &mut String, run: &mut usize| {
        if *run > 0 {
            out.push_str(&format!("    {marker} ... {run} line(s) of body left out\n"));
            *run = 0;
        }
    };

    for (i, line) in block.iter().enumerate() {
        if kept[i] {
            flush(&mut out, &mut dropped_run);
            out.push_str(strip_pad(line, &pad).trim_end());
            out.push('\n');
        } else if !line.trim().is_empty() {
            dropped_run += 1;
            dropped_any = true;
        }
    }
    flush(&mut out, &mut dropped_run);

    (out.trim_end().to_string(), dropped_any)
}

/// Keep lines whose brace depth at the start of the line is at most one
/// below the container's own — the container's braces and its members'
/// declarations, not their bodies.
fn braced_member_mask(lines: &[&str], start: usize, end: usize) -> Vec<bool> {
    let mut scan = Scan::default();
    let mut mask = Vec::with_capacity(end - start + 1);

    for line in lines.iter().take(end + 1).skip(start) {
        mask.push(scan.depth <= 1);
        scan.finished = false;
        scan.line(line);
    }

    mask
}

/// The indentation equivalent: keep the container's line and its direct
/// children, drop what sits under those.
fn indented_member_mask(lines: &[&str], start: usize, end: usize) -> Vec<bool> {
    let base = indent_width(lines[start]);
    let member = lines
        .iter()
        .take(end + 1)
        .skip(start + 1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_width(l))
        .min()
        .unwrap_or(base);

    (start..=end)
        .map(|i| i == start || indent_width(lines[i]) <= member)
        .collect()
}

fn is_data_kind(kind: &str) -> bool {
    DATA_KINDS.contains(&kind)
}

fn is_container_kind(kind: &str) -> bool {
    CONTAINER_KINDS.contains(&kind)
}

fn is_indentation_scoped(language: &str) -> bool {
    matches!(language, "python" | "ruby")
}

fn comment_prefix(language: &str) -> &'static str {
    if is_indentation_scoped(language) {
        "#"
    } else {
        "//"
    }
}

/// Whether a declaration is part of the repository's public contract.
///
/// Errs towards including: over-inclusion costs tokens, omission costs a
/// wasted job. Only what a language makes unambiguously non-public is
/// dropped.
fn is_public(node: &FileNode, symbol: &Symbol, declaration: &str) -> bool {
    let modifiers = leading_modifiers(declaration);

    match node.language.as_str() {
        // A Rust `impl` block carries the public method surface of a type
        // and is never itself marked `pub`.
        "rust" => symbol.kind == "impl" || modifiers.contains(&"pub"),
        "python" => !symbol.name.starts_with('_'),
        "go" => symbol
            .name
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase()),
        "javascript" | "typescript" => {
            modifiers.contains(&"export") || exported_by_list(&node.source, &symbol.name)
        }
        // Everywhere else `private` is the only unambiguous statement. C#'s
        // implicit `internal`, and an unmarked member's implicit privacy,
        // are left in: a supervisor pasting a little too much is a cost
        // measured in tokens, and the other way round is measured in
        // failed jobs.
        _ => !modifiers.contains(&"private"),
    }
}

/// Tokens from the start of a line for as long as they are modifiers. Stops
/// at the first thing that is not one, so a `private set;` further along a
/// public property's line cannot be mistaken for the property's own
/// visibility.
fn leading_modifiers(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for token in line.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if token.is_empty() {
            continue;
        }
        if MODIFIERS.contains(&token) {
            out.push(token);
        } else {
            break;
        }
    }
    out
}

/// TypeScript and JavaScript can declare privately and export at the bottom
/// of the file. Missing those would omit most of the surface of a module
/// written that way.
fn exported_by_list(source: &str, name: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("export") && contains_word(trimmed, name)
    })
}

fn contains_word(line: &str, word: &str) -> bool {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .any(|token| token == word)
}

fn indent_width(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

fn leading_whitespace(line: &str) -> String {
    line.chars().take_while(|c| c.is_whitespace()).collect()
}

fn strip_pad<'a>(line: &'a str, pad: &str) -> &'a str {
    line.strip_prefix(pad).unwrap_or(line)
}

/// Re-align a span to column zero using its first line's indentation, so a
/// deeply nested C# member does not arrive with eight columns of dead space
/// in front of every line.
fn dedent(block: &[&str]) -> String {
    let Some(first) = block.first() else {
        return String::new();
    };
    let pad = leading_whitespace(first);
    block
        .iter()
        .map(|line| strip_pad(line, &pad).trim_end())
        .collect::<Vec<&str>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn prefix_key(path: &str) -> String {
    normalize_slashes(path)
        .trim_matches('/')
        .to_lowercase()
}

/// Path comparison is case-insensitive because Windows is the primary
/// platform and `--surface-path Src` matching nothing on a `src/` tree
/// would be a silently empty surface — the failure mode this module is
/// against.
fn is_under(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let path = path.to_lowercase();
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Index;

    /// Minimal temp-dir helper; the crate has no dev-dependency on tempfile
    /// and the pinned dependency set is deliberately closed.
    mod tempdir {
        use std::path::{Path, PathBuf};
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = std::env::temp_dir()
                    .join(format!("codemason-surface-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn index_of(files: &[(&str, &str)]) -> (tempdir::Dir, Index) {
        let dir = tempdir::Dir::new();
        for (name, body) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        let index = Index::build(dir.path()).expect("index builds");
        (dir, index)
    }

    fn surface_of(files: &[(&str, &str)], path: Option<&str>) -> Surface {
        let (_dir, index) = index_of(files);
        extract(index.graph(), "repo", path)
    }

    fn text_of(files: &[(&str, &str)], path: Option<&str>) -> String {
        render(&surface_of(files, path))
    }

    /// The regression test. A regex that stopped at the newline emitted
    /// `pub enum LoopOutcome` and a job invented the members; every variant
    /// must survive.
    #[test]
    fn an_enums_variants_all_survive() {
        let text = text_of(
            &[(
                "src/loop.rs",
                "/// Why the loop stopped.\n\
                 #[derive(Debug)]\n\
                 pub enum LoopOutcome {\n\
                 \x20   Completed,\n\
                 \x20   BudgetExceeded,\n\
                 \x20   MaxIterations,\n\
                 }\n",
            )],
            None,
        );

        assert!(text.contains("LoopOutcome"), "missing the type: {text}");
        for variant in ["Completed", "BudgetExceeded", "MaxIterations"] {
            assert!(
                text.contains(variant),
                "variant {variant} was truncated away: {text}"
            );
        }
    }

    #[test]
    fn a_structs_fields_all_survive() {
        let text = text_of(
            &[(
                "src/limits.rs",
                "pub struct LoopLimits {\n\
                 \x20   pub max_iterations: u32,\n\
                 \x20   pub budget_tokens: u64,\n\
                 \x20   pub budget_usd: Option<f64>,\n\
                 }\n",
            )],
            None,
        );

        for field in ["max_iterations", "budget_tokens", "budget_usd"] {
            assert!(text.contains(field), "field {field} missing: {text}");
        }
    }

    /// The other half of the original failure: a C# positional record, whose
    /// parameters live past the first newline and end on a `;` rather than a
    /// brace.
    #[test]
    fn a_positional_record_keeps_its_parameters() {
        let text = text_of(
            &[
                (
                    "src/Limits.cs",
                    "namespace Codemason;\n\n\
                     public sealed record LoopLimits(\n\
                     \x20   int MaxIterations,\n\
                     \x20   long BudgetTokens,\n\
                     \x20   decimal? BudgetUsd);\n",
                ),
                ("src/keep.rs", "pub fn keep() {}\n"),
            ],
            None,
        );

        for parameter in ["MaxIterations", "BudgetTokens", "BudgetUsd"] {
            assert!(
                text.contains(parameter),
                "parameter {parameter} was truncated away: {text}"
            );
        }
    }

    /// A function's body is not part of its contract, but every parameter
    /// and the return type are — including across a line break.
    #[test]
    fn a_multiline_function_signature_is_joined_whole() {
        let text = text_of(
            &[(
                "src/run.rs",
                "pub fn run(\n\
                 \x20   config: &LoopConfig,\n\
                 \x20   budget: u64,\n\
                 ) -> Result<LoopExit, Error> {\n\
                 \x20   unimplemented!()\n\
                 }\n",
            )],
            None,
        );

        assert!(text.contains("config: &LoopConfig"), "{text}");
        assert!(text.contains("budget: u64"), "{text}");
        assert!(text.contains("-> Result<LoopExit, Error>"), "{text}");
        assert!(!text.contains("unimplemented!"), "body leaked: {text}");
    }

    #[test]
    fn surface_path_restricts_the_output() {
        let files = [
            ("keep/kept.rs", "pub fn kept_symbol() {}\n"),
            ("drop/dropped.rs", "pub fn dropped_symbol() {}\n"),
        ];

        let whole = text_of(&files, None);
        assert!(whole.contains("kept_symbol"));
        assert!(whole.contains("dropped_symbol"));

        let restricted = surface_of(&files, Some("keep"));
        assert_eq!(restricted.stats.files, 1, "{:?}", restricted.files);
        let text = render(&restricted);
        assert!(text.contains("kept_symbol"), "{text}");
        assert!(!text.contains("dropped_symbol"), "{text}");
        assert!(!text.contains("drop/dropped.rs"), "{text}");
    }

    #[test]
    fn a_subpath_matching_nothing_yields_an_empty_surface_rather_than_the_whole_repo() {
        let surface = surface_of(&[("src/a.rs", "pub fn a() {}\n")], Some("nowhere"));
        assert_eq!(surface.stats.files, 0);
        assert_eq!(surface.stats.symbols, 0);
    }

    /// A file the engine parses but finds nothing in must be reported as
    /// having nothing, not omitted — a reader cannot otherwise tell "no
    /// public API" from "never looked at".
    #[test]
    fn a_file_with_no_recognised_symbols_is_reported_not_dropped() {
        let text = text_of(
            &[
                ("src/empty.rs", "// Nothing here yet.\n\n// Really nothing.\n"),
                ("src/real.rs", "pub fn real() {}\n"),
            ],
            None,
        );

        assert!(text.contains("src/empty.rs"), "{text}");
        assert!(text.contains("no public symbols recognised"), "{text}");
        assert!(text.contains("pub fn real()"), "{text}");
    }

    #[test]
    fn private_declarations_are_left_out() {
        let text = text_of(
            &[(
                "src/mix.rs",
                "pub fn exported() {}\n\nfn internal_helper() {}\n",
            )],
            None,
        );

        assert!(text.contains("exported"), "{text}");
        assert!(!text.contains("internal_helper"), "{text}");
    }

    /// Past the verbatim threshold a container drops implementation bodies —
    /// but never a member declaration, and never without saying so.
    #[test]
    fn a_long_container_keeps_every_member_and_marks_what_it_dropped() {
        let mut source = String::from("pub struct Big;\n\nimpl Big {\n");
        for i in 0..40 {
            source.push_str(&format!(
                "    pub fn method_{i}(&self) -> u32 {{\n        let value = {i};\n        value\n    }}\n"
            ));
        }
        source.push_str("}\n");

        let surface = surface_of(&[("src/big.rs", source.as_str())], None);
        let text = render(&surface);

        for i in 0..40 {
            assert!(
                text.contains(&format!("method_{i}")),
                "member method_{i} was dropped: {text}"
            );
        }
        assert!(!text.contains("let value"), "bodies leaked: {text}");
        assert!(text.contains("line(s) of body left out"), "{text}");
        assert_eq!(surface.stats.shortened, 1, "elision must be counted");
    }

    /// Nothing is shortened unless a body was genuinely dropped, so a
    /// supervisor can assert on the count.
    #[test]
    fn a_short_repository_reports_nothing_shortened() {
        let surface = surface_of(
            &[(
                "src/small.rs",
                "pub enum E {\n    A,\n    B,\n}\n\npub fn f(a: u32) -> u32 { a }\n",
            )],
            None,
        );
        assert_eq!(surface.stats.shortened, 0, "{:?}", surface.files);
    }

    #[test]
    fn the_json_form_carries_the_same_symbols() {
        let surface = surface_of(
            &[("src/loop.rs", "pub enum E {\n    A,\n    B,\n}\n")],
            None,
        );
        let encoded = serde_json::to_string(&surface).expect("serialises");
        let decoded: serde_json::Value = serde_json::from_str(&encoded).expect("round-trips");

        assert_eq!(decoded["stats"]["files"], 1);
        assert_eq!(decoded["stats"]["symbols"], 1);
        let text = decoded["files"][0]["symbols"][0]["text"]
            .as_str()
            .expect("symbol text");
        assert!(text.contains('A') && text.contains('B'), "{text}");
    }

    /// A lifetime is not a character literal. Getting that wrong would put
    /// the brace scanner inside a string for the rest of the file and take
    /// the span with it.
    #[test]
    fn a_lifetime_does_not_derail_the_span_scanner() {
        let text = text_of(
            &[(
                "src/borrow.rs",
                "pub struct Borrowed<'a> {\n\
                 \x20   pub name: &'a str,\n\
                 \x20   pub sep: char,\n\
                 }\n\n\
                 pub fn after() {}\n",
            )],
            None,
        );

        assert!(text.contains("pub name: &'a str"), "{text}");
        assert!(text.contains("pub sep: char"), "{text}");
        assert!(text.contains("pub fn after()"), "the span ran on: {text}");
    }

    #[test]
    fn python_uses_indentation_for_its_spans() {
        let text = text_of(
            &[(
                "src/api.py",
                "class Client:\n\
                 \x20   def send(self, message):\n\
                 \x20       return message\n\
                 \n\
                 def _private():\n\
                 \x20   pass\n",
            )],
            None,
        );

        assert!(text.contains("class Client"), "{text}");
        assert!(text.contains("def send(self, message)"), "{text}");
        assert!(!text.contains("_private"), "{text}");
    }
}
