//! Built-in coding tools: read, bash, powershell (Windows), edit, write, grep, find, ls, and
//! the factories embedders build them with (upstream `core/tools/index.ts`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use pi_agent::SharedTool;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::settings::SettingsManager;

mod bash;
mod edit;
pub mod edit_diff;
mod find;
mod grep;
mod ls;
pub mod mutation_queue;
mod output;
pub mod path_utils;
mod powershell;
mod read;
mod shell;
mod shell_process;
pub mod truncate;
mod write;

#[cfg(test)]
mod tests;

pub use bash::{BASH_TOOL_CONFIG, BashTool, BashToolOptions};
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use output::{FullOutput, OutputAccumulator, OutputSnapshot};
pub use powershell::{LocalPowerShellOperations, POWERSHELL_TOOL_CONFIG, PowerShellTool, PowerShellToolOptions};
pub(crate) use read::detect_supported_image_mime_type;
pub use read::{ReadTool, ReadToolOptions, read_output_schema};
pub use shell::{
    SessionEnvironmentFn, ShellSpawnContext, ShellSpawnHook, ShellTool, ShellToolConfig, session_environment,
    set_session_environment, shell_output_schema,
};
pub use shell_process::{
    CommandTransport, LocalShellOperations, POWERSHELL_ARGS, ShellConfig, ShellDataCallback, ShellExecError,
    ShellExecOptions, ShellOperations, bash_shell_config, powershell_shell_config, shell_env,
};
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
/// Every built-in tool; `powershell` is registered on Windows only.
#[cfg(windows)]
pub const ALL_TOOLS: &[&str] = &["read", "bash", "powershell", "edit", "write", "grep", "find", "ls"];
#[cfg(not(windows))]
pub const ALL_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls"];
/// Tools of [`create_coding_tools`].
pub const CODING_TOOLS: &[&str] = &["read", "bash", "edit", "write"];
/// Tools of [`create_read_only_tools`].
pub const READ_ONLY_TOOLS: &[&str] = &["read", "grep", "find", "ls"];

const TOOL_INFOS: &[ToolInfo] = &[
    ToolInfo {
        name: "read",
        snippet: "Read file contents",
        guidelines: &["Use read to examine files instead of cat or sed."],
    },
    ToolInfo { name: "bash", snippet: BASH_TOOL_CONFIG.prompt_snippet, guidelines: BASH_TOOL_CONFIG.prompt_guidelines },
    ToolInfo {
        name: "powershell",
        snippet: POWERSHELL_TOOL_CONFIG.prompt_snippet,
        guidelines: POWERSHELL_TOOL_CONFIG.prompt_guidelines,
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

fn builtin_snippet(name: &str) -> Option<&'static str> {
    tool_info(name).map(|info| info.snippet)
}

fn builtin_guidelines(name: &str) -> Vec<String> {
    tool_info(name).map(|info| info.guidelines.iter().map(|rule| rule.to_string()).collect()).unwrap_or_default()
}

/// Constrained sampling the file and shell tools request: a strict schema where supported.
fn prefer_json_schema() -> Value {
    json!({ "type": "json_schema", "strict": "prefer" })
}

/// Options of the built-in tools (upstream `ToolsOptions`). The other tools have none.
#[derive(Clone, Default)]
pub struct ToolsOptions {
    pub read: ReadToolOptions,
    pub bash: BashToolOptions,
    pub powershell: PowerShellToolOptions,
}

impl ToolsOptions {
    /// `images.autoResize`, `shellPath`, and `shellCommandPrefix` from the settings.
    pub fn from_settings(settings: &SettingsManager) -> Self {
        Self {
            read: ReadToolOptions { auto_resize_images: settings.image_auto_resize(), resize_options: None },
            bash: BashToolOptions {
                shell_path: settings.shell_path(),
                command_prefix: settings.shell_command_prefix(),
                ..BashToolOptions::default()
            },
            powershell: PowerShellToolOptions::default(),
        }
    }
}

pub fn create_read_tool(cwd: &Path, options: ReadToolOptions) -> SharedTool {
    Arc::new(ReadTool::with_options(cwd.to_path_buf(), options))
}

pub fn create_bash_tool(cwd: &Path, options: BashToolOptions) -> SharedTool {
    Arc::new(ShellTool::bash(cwd.to_path_buf(), options))
}

/// The PowerShell tool. Its local operations fail outside Windows; custom operations work anywhere.
pub fn create_powershell_tool(cwd: &Path, options: PowerShellToolOptions) -> SharedTool {
    Arc::new(ShellTool::powershell(cwd.to_path_buf(), options))
}

pub fn create_edit_tool(cwd: &Path) -> SharedTool {
    Arc::new(EditTool::new(cwd.to_path_buf()))
}

pub fn create_write_tool(cwd: &Path) -> SharedTool {
    Arc::new(WriteTool::new(cwd.to_path_buf()))
}

pub fn create_grep_tool(cwd: &Path) -> SharedTool {
    Arc::new(GrepTool::new(cwd.to_path_buf()))
}

pub fn create_find_tool(cwd: &Path) -> SharedTool {
    Arc::new(FindTool::new(cwd.to_path_buf()))
}

pub fn create_ls_tool(cwd: &Path) -> SharedTool {
    Arc::new(LsTool::new(cwd.to_path_buf()))
}

/// The built-in tool `name` (one of [`ALL_TOOLS`]), or `None` for other names.
pub fn create_tool_with_options(name: &str, cwd: &Path, options: &ToolsOptions) -> Option<SharedTool> {
    let tool = match name {
        "read" => create_read_tool(cwd, options.read),
        "bash" => create_bash_tool(cwd, options.bash.clone()),
        "powershell" if cfg!(windows) => create_powershell_tool(cwd, options.powershell.clone()),
        "edit" => create_edit_tool(cwd),
        "write" => create_write_tool(cwd),
        "grep" => create_grep_tool(cwd),
        "find" => create_find_tool(cwd),
        "ls" => create_ls_tool(cwd),
        _ => return None,
    };
    Some(tool)
}

/// The built-in tool `name` with default options.
pub fn create_tool(name: &str, cwd: &Path) -> Option<SharedTool> {
    create_tool_with_options(name, cwd, &ToolsOptions::default())
}

pub fn create_tools_with_options(
    names: &[&str],
    cwd: &Path,
    options: &ToolsOptions,
) -> anyhow::Result<Vec<SharedTool>> {
    names
        .iter()
        .map(|name| create_tool_with_options(name, cwd, options).ok_or_else(|| anyhow!("Unknown tool name: {name}")))
        .collect()
}

pub fn create_tools(names: &[&str], cwd: &Path) -> anyhow::Result<Vec<SharedTool>> {
    create_tools_with_options(names, cwd, &ToolsOptions::default())
}

/// read, bash, edit, and write (upstream `createCodingTools`).
pub fn create_coding_tools(cwd: &Path, options: &ToolsOptions) -> Vec<SharedTool> {
    CODING_TOOLS.iter().filter_map(|name| create_tool_with_options(name, cwd, options)).collect()
}

/// read, grep, find, and ls (upstream `createReadOnlyTools`).
pub fn create_read_only_tools(cwd: &Path, options: &ToolsOptions) -> Vec<SharedTool> {
    READ_ONLY_TOOLS.iter().filter_map(|name| create_tool_with_options(name, cwd, options)).collect()
}

/// Every tool of [`ALL_TOOLS`] by name (upstream `createAllTools`).
pub fn create_all_tools(cwd: &Path, options: &ToolsOptions) -> BTreeMap<&'static str, SharedTool> {
    ALL_TOOLS.iter().filter_map(|name| Some((*name, create_tool_with_options(name, cwd, options)?))).collect()
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
