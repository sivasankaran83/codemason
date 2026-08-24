//! Path safety and text handling shared by every tool: canonicalising and
//! confining paths to the repository root, presenting file content to the
//! model as normalised LF text while remembering how to restore the
//! original line-ending/BOM convention on write, and detecting content that
//! looks like it elided unchanged code instead of supplying it in full.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Replace backslashes with forward slashes. Every tool argument and result
/// uses forward slashes; platform paths are produced only at the filesystem
/// boundary (`to_repo_relative`'s return value).
pub fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

/// Resolve a tool-supplied, forward-slash path against the repository root,
/// rejecting anything that canonicalises outside it. Returns an error value
/// rather than panicking on a missing file or an escaping path — both are
/// things the calling tool can report back to the model.
///
/// `std::fs::canonicalize` on Windows returns a `\\?\`-prefixed verbatim
/// path, which is what actually lifts the 260-character `MAX_PATH` limit;
/// no separate long-path opt-in is needed as long as every filesystem call
/// goes through the path this function returns.
pub fn to_repo_relative(repo_root: &Path, raw: &str) -> Result<PathBuf, Error> {
    let root_canonical = repo_root
        .canonicalize()
        .map_err(|_| Error::PathEscapesRepo {
            path: raw.to_string(),
        })?;

    let normalized = normalize_slashes(raw);
    let trimmed = normalized.trim_start_matches('/');
    let candidate = root_canonical.join(trimmed);

    let canonical = candidate.canonicalize().map_err(|_| Error::PathEscapesRepo {
        path: raw.to_string(),
    })?;

    if !canonical.starts_with(&root_canonical) {
        return Err(Error::PathEscapesRepo {
            path: raw.to_string(),
        });
    }

    Ok(canonical)
}

/// Same traversal-rejection contract as `to_repo_relative`, but tolerant of a
/// target file that does not exist yet: only the parent directory is
/// required to exist (it is created if absent) and resolve inside the
/// repository root. `write_file` is the only caller — every read-path tool
/// keeps using `to_repo_relative`, which correctly requires the full path to
/// already exist.
///
/// Traversal is rejected lexically, *before* any directory is created:
/// `canonicalize` cannot be used for this check up front because it requires
/// the path to already exist, so a `..` component is walked against a stack
/// seeded with the (already-canonical) root's own components, refusing to
/// pop below that seed. Only once the candidate is proven to lexically stay
/// inside the root does `create_dir_all` run, and the result is canonicalized
/// again afterward as defense in depth against a symlink planted inside the
/// repository.
pub fn to_repo_relative_for_write(repo_root: &Path, raw: &str) -> Result<PathBuf, Error> {
    let escapes = || Error::PathEscapesRepo {
        path: raw.to_string(),
    };

    let root_canonical = repo_root.canonicalize().map_err(|_| escapes())?;

    let normalized = normalize_slashes(raw);
    let trimmed = normalized.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(escapes());
    }

    let root_len = root_canonical.components().count();
    let mut stack: Vec<std::path::Component> = root_canonical.components().collect();
    for component in Path::new(trimmed).components() {
        match component {
            std::path::Component::Normal(seg) => stack.push(std::path::Component::Normal(seg)),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if stack.len() <= root_len {
                    return Err(escapes());
                }
                stack.pop();
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(escapes());
            }
        }
    }
    if stack.len() <= root_len {
        return Err(escapes());
    }

    let candidate: PathBuf = stack.iter().collect();
    let file_name = candidate.file_name().ok_or_else(escapes)?.to_os_string();
    let parent = candidate.parent().ok_or_else(escapes)?;

    std::fs::create_dir_all(parent).map_err(|_| escapes())?;

    let parent_canonical = parent.canonicalize().map_err(|_| escapes())?;
    if !parent_canonical.starts_with(&root_canonical) {
        return Err(escapes());
    }

    Ok(parent_canonical.join(file_name))
}

/// The repo-root-relative string a canonical path resolves to, in whatever
/// separator convention `std::path::Path::to_string_lossy` produces on this
/// platform. This intentionally matches the engine's own chunk/graph keys
/// (`engine::index::create` builds them the same way, via
/// `strip_prefix(display_root)` on a path canonicalised the same way) —
/// looking up `DependencyGraph::deps` needs this exact form, not the
/// forward-slash form used for display to the model.
pub fn repo_relative_key(repo_root: &Path, canonical: &Path) -> Result<String, Error> {
    let root_canonical = repo_root
        .canonicalize()
        .map_err(|_| Error::PathEscapesRepo {
            path: canonical.to_string_lossy().to_string(),
        })?;
    canonical
        .strip_prefix(&root_canonical)
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|_| Error::PathEscapesRepo {
            path: canonical.to_string_lossy().to_string(),
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

#[derive(Debug, Clone)]
pub struct ReadPresentation {
    /// LF-normalised, BOM-stripped content, ready for line-numbering and
    /// display to the model.
    pub content: String,
    pub line_ending: LineEnding,
    pub had_bom: bool,
}

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Detect the dominant line ending and a leading UTF-8 BOM, and present the
/// content as plain LF text. The per-file convention is recorded so
/// `restore_for_write` can put it back exactly on write.
pub fn read_for_model(bytes: &[u8]) -> ReadPresentation {
    let (had_bom, rest) = match bytes.strip_prefix(&UTF8_BOM) {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };

    let raw = String::from_utf8_lossy(rest).into_owned();
    let crlf_count = raw.matches("\r\n").count();
    let total_newlines = raw.matches('\n').count();
    let lf_only_count = total_newlines.saturating_sub(crlf_count);

    let line_ending = if crlf_count > 0 && crlf_count >= lf_only_count {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };

    let content = raw.replace("\r\n", "\n");

    ReadPresentation {
        content,
        line_ending,
        had_bom,
    }
}

/// Restore a file's recorded line-ending and BOM convention over LF content
/// before writing it back to disk.
pub fn restore_for_write(content_lf: &str, line_ending: LineEnding, had_bom: bool) -> Vec<u8> {
    let body = match line_ending {
        LineEnding::Crlf => content_lf.replace('\n', "\r\n"),
        LineEnding::Lf => content_lf.to_string(),
    };

    let mut out = Vec::with_capacity(body.len() + UTF8_BOM.len());
    if had_bom {
        out.extend_from_slice(&UTF8_BOM);
    }
    out.extend_from_slice(body.as_bytes());
    out
}

/// Markers that suggest the model wrote "the rest is unchanged" instead of
/// the complete file. Matched case-insensitively as substrings.
const ELISION_MARKERS: [&str; 3] = ["... rest of", "unchanged", "... existing"];

/// True when `new_content` is under half the length of `existing` *and*
/// contains one of the elision markers — either alone is normal (a genuine
/// edit can shrink a file; the word "unchanged" can appear in real code).
pub fn looks_elided(existing: &str, new_content: &str) -> bool {
    if existing.is_empty() {
        return false;
    }
    if new_content.len() >= existing.len() / 2 {
        return false;
    }
    let lower = new_content.to_lowercase();
    ELISION_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Documents and enforces the stdout-UTF-8 discipline: every stdout write in
/// this binary goes through `write_all` with pre-encoded UTF-8 bytes
/// (`write_stdout`), never through a locale-dependent formatter. No FFI call
/// is needed to reach this — `std::io::Stdout` on Windows already routes
/// through `WriteConsoleW` (UTF-16, codepage-independent) when attached to a
/// real console, and raw UTF-8 bytes are exactly what a redirected pipe or
/// file needs regardless of platform.
pub fn init_stdout_utf8() {}

/// Write pre-encoded UTF-8 text to stdout, flushing immediately.
pub fn write_stdout(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(text.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codemason-text-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn traversal_outside_root_is_rejected_not_panicked() {
        let root_parent = temp_dir("traversal-parent");
        let root = root_parent.join("repo");
        fs::create_dir_all(&root).unwrap();
        let outside_file = root_parent.join("secret.txt");
        fs::write(&outside_file, "top secret").unwrap();

        let err = to_repo_relative(&root, "../secret.txt").expect_err("should reject traversal");
        assert!(matches!(err, Error::PathEscapesRepo { .. }));
    }

    #[test]
    fn missing_path_is_an_error_not_a_panic() {
        let root = temp_dir("missing");
        let err = to_repo_relative(&root, "does/not/exist.txt").expect_err("should error");
        assert!(matches!(err, Error::PathEscapesRepo { .. }));
    }

    #[test]
    fn path_within_root_resolves() {
        let root = temp_dir("within");
        fs::write(root.join("a.txt"), "hi").unwrap();
        let resolved = to_repo_relative(&root, "a.txt").expect("should resolve");
        assert!(resolved.ends_with("a.txt"));
    }

    #[test]
    fn crlf_round_trips_crlf() {
        let original = b"line one\r\nline two\r\nline three\r\n".to_vec();
        let presentation = read_for_model(&original);
        assert_eq!(presentation.line_ending, LineEnding::Crlf);
        assert!(!presentation.content.contains('\r'));

        let restored = restore_for_write(&presentation.content, presentation.line_ending, presentation.had_bom);
        assert_eq!(restored, original);
    }

    #[test]
    fn bom_round_trips_with_bom() {
        let mut original = UTF8_BOM.to_vec();
        original.extend_from_slice(b"content\nhere\n");
        let presentation = read_for_model(&original);
        assert!(presentation.had_bom);

        let restored = restore_for_write(&presentation.content, presentation.line_ending, presentation.had_bom);
        assert_eq!(restored, original);
        assert!(restored.starts_with(&UTF8_BOM));
    }

    #[test]
    fn lf_gains_no_crlf() {
        let original = b"line one\nline two\nline three\n".to_vec();
        let presentation = read_for_model(&original);
        assert_eq!(presentation.line_ending, LineEnding::Lf);

        let restored = restore_for_write(&presentation.content, presentation.line_ending, presentation.had_bom);
        assert_eq!(restored, original);
        assert!(!restored.windows(2).any(|w| w == b"\r\n"));
    }

    #[test]
    fn elision_fires_at_40_percent_length_with_a_marker() {
        let existing = "a".repeat(1000);
        let new_content = format!("{}\n// ... rest of the file unchanged", "a".repeat(390));
        assert!(new_content.len() < existing.len() / 2);
        assert!(looks_elided(&existing, &new_content));
    }

    #[test]
    fn elision_does_not_fire_without_a_marker() {
        let existing = "a".repeat(1000);
        let new_content = "b".repeat(400);
        assert!(!looks_elided(&existing, &new_content));
    }

    #[test]
    fn elision_does_not_fire_above_half_length_even_with_a_marker() {
        let existing = "a".repeat(1000);
        let new_content = format!("{} ... rest of it", "a".repeat(600));
        assert!(!looks_elided(&existing, &new_content));
    }

    #[test]
    fn path_over_260_characters_is_readable() {
        let root = temp_dir("longpath");
        let mut dir = root.clone();
        let mut rel = String::new();
        // Build enough nested directories to push the total path length
        // comfortably past Windows' historical 260-character MAX_PATH.
        for i in 0..12 {
            let segment = format!("segment-{i:03}-abcdefghijklmnopqrstuvwxyz");
            dir = dir.join(&segment);
            if !rel.is_empty() {
                rel.push('/');
            }
            rel.push_str(&segment);
        }
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("deep.txt");
        fs::write(&file_path, "deep content").unwrap();
        rel.push_str("/deep.txt");

        assert!(root.join(&rel).to_string_lossy().len() > 260);

        let resolved = to_repo_relative(&root, &rel).expect("long path should resolve");
        let content = fs::read_to_string(&resolved).expect("long path should be readable");
        assert_eq!(content, "deep content");
    }

    #[test]
    fn for_write_rejects_traversal_before_creating_anything() {
        let root_parent = temp_dir("write-traversal-parent");
        let root = root_parent.join("repo");
        fs::create_dir_all(&root).unwrap();

        let err = to_repo_relative_for_write(&root, "../escape.txt")
            .expect_err("should reject traversal");
        assert!(matches!(err, Error::PathEscapesRepo { .. }));
        assert!(!root_parent.join("escape.txt").exists());
    }

    #[test]
    fn for_write_resolves_a_new_file_and_creates_parent_dirs() {
        let root = temp_dir("write-new-file");
        let resolved = to_repo_relative_for_write(&root, "nested/dir/new.txt")
            .expect("should resolve a not-yet-existing file");
        assert!(resolved.parent().unwrap().is_dir());
        assert!(!resolved.exists());
        assert_eq!(resolved.file_name().unwrap(), "new.txt");
    }
}
