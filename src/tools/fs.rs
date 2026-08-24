//! `read_file`, `list_files` and `write_file`. `run_command` lives in the
//! sibling `exec.rs`.

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

use crate::text::{self, normalize_slashes};
use crate::tools::{ToolContext, ToolOutcome};

const DEFAULT_MAX_RESULTS: usize = 100;
const READ_LINE_CAP: usize = 2000;
const BINARY_PROBE_BYTES: usize = 8192;
const WRITE_MAX_BYTES: usize = 500 * 1024;

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

/// Whole-file replacement. Creates parent directories and the file itself if
/// absent. Rejects (as a tool-result error the model can act on, never a
/// panic or a failed run): content over 500 KB, a path outside the repo
/// root, content that looks elided, and any attempt under `--dry-run` — each
/// check runs before anything touches the filesystem.
pub fn write_file(ctx: &ToolContext, path: &str, content: &str) -> ToolOutcome {
    if ctx.dry_run {
        return ToolOutcome::Error(format!(
            "write_file: --dry-run is set, the write to {path} was simulated and not performed"
        ));
    }

    if content.len() > WRITE_MAX_BYTES {
        return ToolOutcome::Error(format!(
            "content for {path} is {} bytes, over the {WRITE_MAX_BYTES}-byte limit; write it in smaller pieces or reduce it",
            content.len()
        ));
    }

    let resolved = match text::to_repo_relative_for_write(ctx.repo_root, path) {
        Ok(p) => p,
        Err(err) => return ToolOutcome::Error(err.to_string()),
    };

    let (line_ending, had_bom) = match std::fs::read(&resolved) {
        Ok(existing_bytes) => {
            let presentation = text::read_for_model(&existing_bytes);
            if text::looks_elided(&presentation.content, content) {
                return ToolOutcome::Error(format!(
                    "content for {path} looks like it elided unchanged code (short and containing a marker like \"... rest of\" or \"unchanged\"); supply the complete file content"
                ));
            }
            (presentation.line_ending, presentation.had_bom)
        }
        Err(_) => (text::LineEnding::Lf, false),
    };

    let bytes = text::restore_for_write(content, line_ending, had_bom);
    if let Err(err) = std::fs::write(&resolved, &bytes) {
        return ToolOutcome::Error(format!("failed to write {path}: {err}"));
    }

    ToolOutcome::Ok(format!("wrote {} bytes to {path}", bytes.len()))
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
            dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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

    #[test]
    fn write_file_creates_a_new_file_with_parent_dirs() {
        let dir = temp_repo("write-new");
        std::fs::write(dir.join("seed.rs"), "fn main() {}\n").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };

        let outcome = write_file(&ctx, "nested/dir/new.txt", "hello\nworld\n");
        assert!(matches!(outcome, ToolOutcome::Ok(_)), "{outcome:?}");
        let content = std::fs::read_to_string(dir.join("nested/dir/new.txt")).unwrap();
        assert_eq!(content, "hello\nworld\n");
    }

    #[test]
    fn write_file_preserves_crlf_and_bom() {
        let dir = temp_repo("write-crlf-bom");
        let mut original = vec![0xEFu8, 0xBB, 0xBF];
        original.extend_from_slice(b"one\r\ntwo\r\n");
        std::fs::write(dir.join("a.txt"), &original).unwrap();
        std::fs::write(dir.join("seed.rs"), "fn main() {}\n").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };

        let outcome = write_file(&ctx, "a.txt", "one\ntwo\nthree\n");
        assert!(matches!(outcome, ToolOutcome::Ok(_)), "{outcome:?}");
        let written = std::fs::read(dir.join("a.txt")).unwrap();
        assert!(written.starts_with(&[0xEF, 0xBB, 0xBF]));
        let text = String::from_utf8_lossy(&written[3..]);
        assert_eq!(text, "one\r\ntwo\r\nthree\r\n");
    }

    #[test]
    fn write_file_rejects_elided_content_and_leaves_file_unmodified() {
        let dir = temp_repo("write-elision");
        let existing = "a".repeat(1000);
        std::fs::write(dir.join("big.rs"), &existing).unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };

        let elided = format!("{}\n// ... rest of the file unchanged", "a".repeat(390));
        let outcome = write_file(&ctx, "big.rs", &elided);
        assert!(matches!(outcome, ToolOutcome::Error(_)));
        let unchanged = std::fs::read_to_string(dir.join("big.rs")).unwrap();
        assert_eq!(unchanged, existing);
    }

    #[test]
    fn write_file_rejects_content_over_500kb() {
        let dir = temp_repo("write-too-big");
        std::fs::write(dir.join("seed.rs"), "fn main() {}\n").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };

        let huge = "a".repeat(500 * 1024 + 1);
        let outcome = write_file(&ctx, "huge.txt", &huge);
        assert!(matches!(outcome, ToolOutcome::Error(_)));
        assert!(!dir.join("huge.txt").exists());
    }

    #[test]
    fn write_file_under_dry_run_touches_nothing() {
        let dir = temp_repo("write-dry-run");
        std::fs::write(dir.join("seed.rs"), "fn main() {}\n").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: true,
        };

        let outcome = write_file(&ctx, "new.txt", "hello\n");
        assert!(matches!(outcome, ToolOutcome::Error(_)));
        assert!(!dir.join("new.txt").exists());
    }
}
