//! `read_file` and `list_files`. `write_file` and `run_command` are
//! registered in `tools::mod`'s schema list but stubbed there — WP4 gives
//! them real implementations (in this file and a new `exec.rs`
//! respectively) without changing the schemas.

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

use crate::text::{self, normalize_slashes};
use crate::tools::{ToolContext, ToolOutcome};

const DEFAULT_MAX_RESULTS: usize = 100;
const READ_LINE_CAP: usize = 2000;
const BINARY_PROBE_BYTES: usize = 8192;

pub fn read_file(ctx: &ToolContext, path: &str, start_line: i64, end_line: i64) -> ToolOutcome {
    let resolved = match text::to_repo_relative(ctx.repo_root, path) {
        Ok(p) => p,
        Err(err) => return ToolOutcome::Error(err.to_string()),
    };

    let bytes = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(err) => return ToolOutcome::Error(format!("failed to read {path}: {err}")),
    };

    let probe_len = bytes.len().min(BINARY_PROBE_BYTES);
    if bytes[..probe_len].contains(&0u8) {
        return ToolOutcome::Error(format!("{path} looks like a binary file (null byte found)"));
    }

    let presentation = text::read_for_model(&bytes);
    let lines: Vec<&str> = presentation.content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return ToolOutcome::Ok(String::new());
    }

    let start = if start_line <= 0 { 1 } else { start_line as usize };
    let end = if end_line <= 0 {
        total
    } else {
        (end_line as usize).min(total)
    };

    if start > total || start > end {
        return ToolOutcome::Error(format!(
            "start_line {start_line} is out of range for a {total}-line file"
        ));
    }

    let mut effective_end = end;
    let mut truncated_by = 0usize;
    if effective_end - start + 1 > READ_LINE_CAP {
        let capped_end = start + READ_LINE_CAP - 1;
        truncated_by = effective_end - capped_end;
        effective_end = capped_end;
    }

    let mut out = String::new();
    for (offset, line) in lines[(start - 1)..effective_end].iter().enumerate() {
        out.push_str(&format!("{}: {}\n", start + offset, line));
    }
    if truncated_by > 0 {
        out.push_str(&format!(
            "... truncated at {READ_LINE_CAP} lines; {truncated_by} more line(s) available\n"
        ));
    }

    ToolOutcome::Ok(out)
}

pub fn list_files(ctx: &ToolContext, path: &str, pattern: &str, max_results: i64) -> ToolOutcome {
    let root_input = if path.trim().is_empty() { "." } else { path };
    let resolved_root = match text::to_repo_relative(ctx.repo_root, root_input) {
        Ok(p) => p,
        Err(err) => return ToolOutcome::Error(err.to_string()),
    };
    if !resolved_root.is_dir() {
        return ToolOutcome::Error(format!("{path} is not a directory"));
    }

    let cap = if max_results <= 0 {
        DEFAULT_MAX_RESULTS
    } else {
        max_results as usize
    };

    let mut exclude_builder = OverrideBuilder::new(&resolved_root);
    exclude_builder.add("!**/.git").ok();
    exclude_builder.add("!**/.agent").ok();
    let exclude_overrides = match exclude_builder.build() {
        Ok(o) => o,
        Err(err) => return ToolOutcome::Error(format!("failed to build exclusions: {err}")),
    };

    let pattern_override = if pattern.trim().is_empty() {
        None
    } else {
        let mut builder = OverrideBuilder::new(&resolved_root);
        if builder.add(pattern).is_err() {
            return ToolOutcome::Error(format!("invalid pattern: {pattern}"));
        }
        match builder.build() {
            Ok(o) => Some(o),
            Err(err) => return ToolOutcome::Error(format!("invalid pattern {pattern}: {err}")),
        }
    };

    let repo_root_canonical = match ctx.repo_root.canonicalize() {
        Ok(p) => p,
        Err(err) => return ToolOutcome::Error(format!("failed to resolve repository root: {err}")),
    };

    let walker = WalkBuilder::new(&resolved_root)
        .overrides(exclude_overrides)
        .hidden(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();

    let mut results = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let entry_path = entry.path();

        if entry_path.components().any(|c| {
            let s = c.as_os_str().to_string_lossy();
            s == ".git" || s == ".agent"
        }) {
            continue;
        }

        if let Some(overrides) = &pattern_override {
            if !overrides.matched(entry_path, false).is_whitelist() {
                continue;
            }
        }

        let relative = entry_path
            .strip_prefix(&repo_root_canonical)
            .unwrap_or(entry_path);
        results.push(normalize_slashes(&relative.to_string_lossy()));
        if results.len() >= cap {
            break;
        }
    }

    if results.is_empty() {
        return ToolOutcome::Ok(format!("no files found under {path}"));
    }
    ToolOutcome::Ok(results.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("codemason-fs-tool-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_file_numbers_lines_and_respects_range() {
        let dir = temp_repo("read");
        std::fs::write(dir.join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
        };

        let outcome = read_file(&ctx, "a.rs", 2, 3);
        match outcome {
            ToolOutcome::Ok(text) => {
                assert_eq!(text, "2: two\n3: three\n");
            }
            ToolOutcome::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn read_file_refuses_binary_content() {
        let dir = temp_repo("binary");
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3, 0, 5]).unwrap();
        // Give the index something to build over besides the binary file.
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
        };

        let outcome = read_file(&ctx, "bin.dat", 0, 0);
        assert!(matches!(outcome, ToolOutcome::Error(_)));
    }

    #[test]
    fn list_files_excludes_git_and_agent_dirs() {
        let dir = temp_repo("list");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        std::fs::create_dir_all(dir.join(".agent")).unwrap();
        std::fs::write(dir.join(".agent/log.jsonl"), "{}").unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("b.txt"), "hello").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
        };

        let outcome = list_files(&ctx, ".", "", 0);
        match outcome {
            ToolOutcome::Ok(text) => {
                assert!(text.contains("a.rs"));
                assert!(text.contains("b.txt"));
                assert!(!text.contains(".git"));
                assert!(!text.contains(".agent"));
            }
            ToolOutcome::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn list_files_pattern_filters_results() {
        let dir = temp_repo("pattern");
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("b.txt"), "hello").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
        };

        let outcome = list_files(&ctx, ".", "*.rs", 0);
        match outcome {
            ToolOutcome::Ok(text) => {
                assert!(text.contains("a.rs"));
                assert!(!text.contains("b.txt"));
            }
            ToolOutcome::Error(err) => panic!("unexpected error: {err}"),
        }
    }
}
