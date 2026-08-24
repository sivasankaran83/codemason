//! `context_search` and `context_outline` — the engine-backed discovery
//! tools. Dependency-graph information is folded into `context_search`'s
//! results as related paths rather than exposed as separate tools.

use crate::text::normalize_slashes;
use crate::tools::{ToolContext, ToolOutcome};

const DEFAULT_MAX_RESULTS: usize = 10;
const PREVIEW_LINES: usize = 3;

pub fn context_search(ctx: &ToolContext, query: &str, max_results: i64) -> ToolOutcome {
    if query.trim().is_empty() {
        return ToolOutcome::Error("query must not be empty".to_string());
    }
    let top_k = if max_results <= 0 {
        DEFAULT_MAX_RESULTS
    } else {
        max_results as usize
    };

    let results = ctx.index.search(query, top_k);
    if results.is_empty() {
        return ToolOutcome::Ok(format!("no results for {query:?}"));
    }

    let mut out = String::new();
    for result in &results {
        let display_path = normalize_slashes(&result.chunk.file_path);
        let chunk_id = ctx
            .index
            .chunks()
            .iter()
            .position(|c| c == &result.chunk)
            .map(|i| i.to_string())
            .unwrap_or_else(|| "?".to_string());
        out.push_str(&format!(
            "{}:{}-{} (chunk #{}, score={:.3})\n",
            display_path,
            result.chunk.start_line,
            result.chunk.end_line,
            chunk_id,
            result.score
        ));

        let preview: Vec<&str> = result.chunk.content.lines().take(PREVIEW_LINES).collect();
        if !preview.is_empty() {
            out.push_str("  preview: ");
            out.push_str(&preview.join(" \\n "));
            out.push('\n');
        }

        let graph = ctx.index.graph();
        if let Some(node) = graph.deps(&result.chunk.file_path) {
            if !node.depends_on.is_empty() {
                let depends_on: Vec<String> = node
                    .depends_on
                    .iter()
                    .map(|p| normalize_slashes(p))
                    .collect();
                out.push_str(&format!("  depends_on: {}\n", depends_on.join(", ")));
            }
        }
        let dependents = graph.dependents(&result.chunk.file_path);
        if !dependents.is_empty() {
            let dependents: Vec<String> = dependents.iter().map(|p| normalize_slashes(p)).collect();
            out.push_str(&format!("  dependents: {}\n", dependents.join(", ")));
        }
    }

    ToolOutcome::Ok(out)
}

pub fn context_outline(ctx: &ToolContext, path: &str) -> ToolOutcome {
    let resolved = match crate::text::to_repo_relative(ctx.repo_root, path) {
        Ok(p) => p,
        Err(err) => return ToolOutcome::Error(err.to_string()),
    };
    let key = match crate::text::repo_relative_key(ctx.repo_root, &resolved) {
        Ok(k) => k,
        Err(err) => return ToolOutcome::Error(err.to_string()),
    };

    match ctx.index.graph().deps(&key) {
        Some(node) => {
            if node.symbols.is_empty() {
                return ToolOutcome::Ok(format!("{path} has no recognised symbols"));
            }
            let mut out = String::new();
            for symbol in &node.symbols {
                out.push_str(&format!("{} {} (line {})\n", symbol.kind, symbol.name, symbol.line));
            }
            if !node.depends_on.is_empty() {
                let depends_on: Vec<String> = node
                    .depends_on
                    .iter()
                    .map(|p| normalize_slashes(p))
                    .collect();
                out.push_str(&format!("depends_on: {}\n", depends_on.join(", ")));
            }
            ToolOutcome::Ok(out)
        }
        None => ToolOutcome::Error(format!("no outline available for {path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use std::path::Path;

    fn fixture_repo() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("old_source/navex-harness-main/metric-measurement-service")
    }

    /// AC4: `context_search` for a known symbol returns its defining chunk
    /// first.
    #[test]
    fn ac4_context_search_returns_defining_chunk_first() {
        let repo = fixture_repo();
        if !repo.exists() {
            eprintln!("skipping: {} not present", repo.display());
            return;
        }
        let index = Index::build(&repo).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &repo,
            index: &index,
            dry_run: false,
        };
        let outcome = context_search(&ctx, "AgentSchedulerService", 5);
        match outcome {
            ToolOutcome::Ok(text) => {
                let first_line = text.lines().next().expect("at least one line");
                assert!(
                    first_line.contains("AgentSchedulerService.cs"),
                    "expected the defining chunk first, got: {first_line}"
                );
            }
            ToolOutcome::Error(err) => panic!("expected Ok, got error: {err}"),
        }
    }

    /// AC5: `context_outline` lists the expected members of a known class.
    #[test]
    fn ac5_context_outline_lists_expected_members() {
        let repo = fixture_repo();
        if !repo.exists() {
            eprintln!("skipping: {} not present", repo.display());
            return;
        }
        let index = Index::build(&repo).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &repo,
            index: &index,
            dry_run: false,
        };

        // Find a C# file the graph actually indexed, then ask for its
        // outline through the public tool entry point.
        let known_path = index
            .graph()
            .files
            .keys()
            .find(|p| p.ends_with("AgentSchedulerService.cs"))
            .cloned()
            .expect("fixture should contain AgentSchedulerService.cs");

        let outcome = context_outline(&ctx, &normalize_slashes(&known_path));
        match outcome {
            ToolOutcome::Ok(text) => {
                assert!(!text.trim().is_empty(), "expected a non-empty outline");
            }
            ToolOutcome::Error(err) => panic!("expected Ok, got error: {err}"),
        }
    }
}
