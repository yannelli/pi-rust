//! Built-in coding tools: read, bash, edit, write, grep, find, ls.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

mod bash;
mod edit;
pub mod edit_diff;
mod find;
mod grep;
mod ls;
pub mod mutation_queue;
mod output;
pub mod path_utils;
mod read;
pub mod truncate;
mod write;

#[cfg(test)]
mod tests;

pub use bash::{BashTool, set_session_environment};
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub(crate) use read::detect_supported_image_mime_type;
pub use write::WriteTool;

/// System prompt contribution of a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: &'static str,
    pub snippet: &'static str,
    pub guidelines: &'static [&'static str],
}

/// Tools enabled at startup (upstream `DEFAULT_TOOL_NAMES` in settings-manager.ts).
pub const DEFAULT_TOOLS: &[&str] = &["read", "bash", "edit", "write"];
pub const ALL_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls"];

const TOOL_INFOS: &[ToolInfo] = &[
    ToolInfo {
        name: "read",
        snippet: "Read file contents",
        guidelines: &["Use read to examine files instead of cat or sed."],
    },
    ToolInfo {
        name: "bash",
        snippet: "Execute bash commands (ls, grep, find, etc.)",
        guidelines: &["You can inspect PI_* environment variables for current model and session details."],
    },
    ToolInfo {
        name: "edit",
        snippet: "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
        guidelines: &[
            "Use edit for precise changes (edits[].oldText must match exactly)",
            "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
        ],
    },
    ToolInfo {
        name: "write",
        snippet: "Create or overwrite files",
        guidelines: &["Use write only for new files or complete rewrites."],
    },
    ToolInfo { name: "grep", snippet: "Search file contents for patterns (respects .gitignore)", guidelines: &[] },
    ToolInfo { name: "find", snippet: "Find files by glob pattern (respects .gitignore)", guidelines: &[] },
    ToolInfo { name: "ls", snippet: "List directory contents", guidelines: &[] },
];

pub fn tool_info(name: &str) -> Option<ToolInfo> {
    TOOL_INFOS.iter().find(|info| info.name == name).copied()
}

pub fn create_tool(name: &str, cwd: &Path) -> Option<pi_agent::SharedTool> {
    let cwd = cwd.to_path_buf();
    let tool: pi_agent::SharedTool = match name {
        "read" => Arc::new(ReadTool::new(cwd)),
        "bash" => Arc::new(BashTool::new(cwd)),
        "edit" => Arc::new(EditTool::new(cwd)),
        "write" => Arc::new(WriteTool::new(cwd)),
        "grep" => Arc::new(GrepTool::new(cwd)),
        "find" => Arc::new(FindTool::new(cwd)),
        "ls" => Arc::new(LsTool::new(cwd)),
        _ => return None,
    };
    Some(tool)
}

pub fn create_tools(names: &[&str], cwd: &Path) -> anyhow::Result<Vec<pi_agent::SharedTool>> {
    names.iter().map(|name| create_tool(name, cwd).ok_or_else(|| anyhow!("Unknown tool name: {name}"))).collect()
}

pub(crate) fn parse_args<T: DeserializeOwned>(tool: &str, args: Value) -> anyhow::Result<T> {
    serde_json::from_value(args).map_err(|e| anyhow!("Invalid arguments for {tool}: {e}"))
}

pub(crate) fn check_aborted(cancel: &tokio_util::sync::CancellationToken) -> anyhow::Result<()> {
    if cancel.is_cancelled() {
        bail!("Operation aborted");
    }
    Ok(())
}

/// `Some(object)` when the map has entries, `None` otherwise, like upstream's details handling.
pub(crate) fn details_or_none(map: Map<String, Value>) -> Option<Value> {
    (!map.is_empty()).then_some(Value::Object(map))
}

/// Integer view of an optional JSON number argument.
pub(crate) fn as_count(value: Option<f64>) -> Option<i64> {
    value.filter(|v| v.is_finite()).map(|v| v.trunc() as i64)
}
