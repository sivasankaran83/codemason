//! The tool registry — the only place in this binary aware of the tool
//! list. At most seven tools, flat schemas: string and integer properties
//! only, no nested objects, no arrays of objects.
//!
//! The cap was six through WP1–WP5 and was raised to seven by a deliberate,
//! recorded spec amendment when `web_search` was added — see SPEC.md's
//! "Amendment: the seventh tool". The cap still binds: it is a budget, not
//! a formality, because tool count is where weaker models degrade first.

pub mod context;
pub mod exec;
pub mod fs;
pub mod web;

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::index::Index;
use crate::llm;

pub struct ToolContext<'a> {
    pub repo_root: &'a Path,
    pub index: &'a Index,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Ok(String),
    Error(String),
}

impl ToolOutcome {
    pub fn into_text(self) -> String {
        match self {
            ToolOutcome::Ok(text) => text,
            ToolOutcome::Error(text) => format!("error: {text}"),
        }
    }
}

pub enum DispatchResult {
    Ran(ToolOutcome),
    UnknownTool,
    BadArguments(String),
}

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: serde_json::Value,
}

/// The full, fixed seven-tool surface: SPEC.md's T3.3 table in its stated
/// order, followed by `web_search` from the recorded amendment.
pub fn registry() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "context_search",
            description: "Search the repository for relevant code by meaning and keywords. The primary discovery tool — start here.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to search for"},
                    "max_results": {"type": "integer", "description": "Maximum results to return; 0 uses a default"}
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "context_outline",
            description: "List the symbol outline (functions, classes, etc.) of one file.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Repository-relative file path"}
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "read_file",
            description: "Read a file's content, line-numbered. 1-based inclusive line range; 0 means unbounded. Capped at 2000 lines.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Repository-relative file path"},
                    "start_line": {"type": "integer", "description": "1-based start line; 0 means from the beginning"},
                    "end_line": {"type": "integer", "description": "1-based end line, inclusive; 0 means to the end"}
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write_file",
            description: "Replace a file's entire content. Requires complete file content, never fragments or markers such as \"// ... rest unchanged\".",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Repository-relative file path"},
                    "content": {"type": "string", "description": "The complete new file content"}
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "list_files",
            description: "List files under a directory, respecting .gitignore. Always excludes .git/ and .agent/.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Repository-relative directory; empty means the repository root"},
                    "pattern": {"type": "string", "description": "Optional glob pattern to filter results; empty matches everything"},
                    "max_results": {"type": "integer", "description": "Maximum results to return; 0 uses a default"}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "run_command",
            description: "Run a shell command at the repository root, to verify work such as running the build or test suite.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "timeout_seconds": {"type": "integer", "description": "Timeout in seconds; 0 uses the default"}
                },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: "web_search",
            description: "Search the web for information not in this repository, such as library documentation or error messages. Prefer context_search for anything inside the repository.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query"},
                    "max_results": {"type": "integer", "description": "Maximum results to return; 0 uses a default"}
                },
                "required": ["query"]
            }),
        },
    ]
}

pub fn valid_names() -> Vec<&'static str> {
    registry().into_iter().map(|t| t.name).collect()
}

pub fn as_llm_tool_defs() -> Vec<llm::ToolDef> {
    registry()
        .into_iter()
        .map(|spec| llm::ToolDef {
            kind: "function",
            function: llm::FunctionSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                parameters: spec.schema,
            },
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct ContextSearchArgs {
    query: String,
    #[serde(default)]
    max_results: i64,
}

#[derive(Debug, Deserialize)]
struct ContextOutlineArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    start_line: i64,
    #[serde(default)]
    end_line: i64,
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ListFilesArgs {
    #[serde(default)]
    path: String,
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    max_results: i64,
}

#[derive(Debug, Deserialize)]
struct RunCommandArgs {
    command: String,
    #[serde(default)]
    timeout_seconds: i64,
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default)]
    max_results: i64,
}

fn parse_and_run<T: DeserializeOwned>(
    args_json: &str,
    f: impl FnOnce(T) -> ToolOutcome,
) -> DispatchResult {
    match serde_json::from_str::<T>(args_json) {
        Ok(args) => DispatchResult::Ran(f(args)),
        Err(source) => DispatchResult::BadArguments(source.to_string()),
    }
}

/// Execute one tool call. Every branch returns a value the loop can act on —
/// parse failures and unknown names are never a panic.
pub fn dispatch(name: &str, args_json: &str, ctx: &ToolContext) -> DispatchResult {
    match name {
        "context_search" => parse_and_run::<ContextSearchArgs>(args_json, |a| {
            context::context_search(ctx, &a.query, a.max_results)
        }),
        "context_outline" => parse_and_run::<ContextOutlineArgs>(args_json, |a| {
            context::context_outline(ctx, &a.path)
        }),
        "read_file" => parse_and_run::<ReadFileArgs>(args_json, |a| {
            fs::read_file(ctx, &a.path, a.start_line, a.end_line)
        }),
        "write_file" => parse_and_run::<WriteFileArgs>(args_json, |a| {
            fs::write_file(ctx, &a.path, &a.content)
        }),
        "list_files" => parse_and_run::<ListFilesArgs>(args_json, |a| {
            fs::list_files(ctx, &a.path, &a.pattern, a.max_results)
        }),
        "run_command" => parse_and_run::<RunCommandArgs>(args_json, |a| {
            exec::run_command(ctx, &a.command, a.timeout_seconds)
        }),
        "web_search" => parse_and_run::<WebSearchArgs>(args_json, |a| {
            web::web_search(&a.query, a.max_results)
        }),
        _ => DispatchResult::UnknownTool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC6: every tool's JSON schema contains only string and integer
    /// properties at depth one.
    #[test]
    fn every_schema_is_flat_string_or_integer_only() {
        let tools = registry();
        // Raised from six by the recorded amendment that added web_search.
        // Still a hard cap, and still the thing that keeps this binary
        // usable on cheap models — see SPEC.md before raising it again.
        assert!(tools.len() <= 7, "at most seven tools");

        for tool in &tools {
            let properties = tool.schema.get("properties").unwrap_or_else(|| {
                panic!("{} schema has no properties", tool.name)
            });
            let object = properties
                .as_object()
                .unwrap_or_else(|| panic!("{} properties is not an object", tool.name));

            for (prop_name, prop_value) in object {
                assert!(
                    prop_value.get("properties").is_none(),
                    "{}.{} is a nested object",
                    tool.name,
                    prop_name
                );
                assert!(
                    prop_value.get("items").is_none(),
                    "{}.{} is an array",
                    tool.name,
                    prop_name
                );
                let ty = prop_value
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or_else(|| panic!("{}.{} has no type", tool.name, prop_name));
                assert!(
                    ty == "string" || ty == "integer",
                    "{}.{} has type {ty:?}, expected string or integer",
                    tool.name,
                    prop_name
                );
            }
        }
    }

    #[test]
    fn unknown_tool_name_is_reported_not_panicked() {
        let dir = std::env::temp_dir().join(format!("codemason-tools-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::build(&dir).unwrap_or_else(|_| {
            std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
            Index::build(&dir).expect("index build should succeed")
        });
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };
        assert!(matches!(dispatch("nonexistent_tool", "{}", &ctx), DispatchResult::UnknownTool));
    }

    #[test]
    fn malformed_arguments_are_reported_not_panicked() {
        let dir = std::env::temp_dir().join(format!("codemason-tools-test-bad-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        let index = Index::build(&dir).expect("index build should succeed");
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };
        assert!(matches!(
            dispatch("read_file", "not json", &ctx),
            DispatchResult::BadArguments(_)
        ));
        assert!(matches!(
            dispatch("read_file", "{}", &ctx),
            DispatchResult::BadArguments(_)
        ));
    }
}
