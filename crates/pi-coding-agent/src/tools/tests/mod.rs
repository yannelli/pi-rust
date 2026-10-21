//! Tool tests.

use std::path::Path;
use std::sync::Arc;

use pi_agent::{AgentToolResult, ToolUpdateCallback};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{ALL_TOOLS, DEFAULT_TOOLS, create_tool, create_tools, tool_info};

mod bash;
mod edit;
mod factories;
mod file_changes;
mod operations;
mod read_write;
mod search;
mod shell;
mod truncate;

pub(super) fn noop() -> ToolUpdateCallback {
    Arc::new(|_| {})
}

pub(super) async fn run(name: &str, cwd: &Path, args: Value) -> anyhow::Result<AgentToolResult> {
    let tool = create_tool(name, cwd).expect("known tool");
    tool.execute("call-1", args, CancellationToken::new(), noop()).await
}

pub(super) fn text(result: &AgentToolResult) -> String {
    result.content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n")
}

#[test]
fn registry_exposes_all_tools() {
    let dir = tempfile::tempdir().unwrap();
    for name in ALL_TOOLS {
        let tool = create_tool(name, dir.path()).unwrap();
        assert_eq!(tool.name(), *name);
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
        assert!(tool_info(name).is_some_and(|info| !info.snippet.is_empty()));
    }
    assert_eq!(DEFAULT_TOOLS, &["read", "bash", "edit", "write"]);
    assert_eq!(create_tools(DEFAULT_TOOLS, dir.path()).unwrap().len(), 4);
    assert!(create_tools(&["read", "nope"], dir.path()).is_err());
    assert!(create_tool("nope", dir.path()).is_none());
    assert!(tool_info("nope").is_none());
    assert_eq!(tool_info("edit").unwrap().guidelines.len(), 4);
}

#[test]
fn edit_schema_matches_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let params = create_tool("edit", dir.path()).unwrap().parameters();
    assert_eq!(params["required"], serde_json::json!(["path", "edits"]));
    assert_eq!(params["properties"]["edits"]["items"]["required"], serde_json::json!(["oldText", "newText"]));
}
