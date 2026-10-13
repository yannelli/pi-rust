//! `powershell` tool: the shell tool with PowerShell, Windows only (upstream `createPowerShellTool`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use super::shell::{SessionEnvironmentFn, ShellSpawnHook, ShellTool, ShellToolConfig};
use super::shell_process::{
    LocalShellOperations, ShellExecError, ShellExecOptions, ShellOperations, powershell_shell_config,
};

/// Makes PowerShell write UTF-8, so output decodes the same as other shells.
const UTF8_OUTPUT_PREFIX: &str = "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

pub const POWERSHELL_TOOL_CONFIG: ShellToolConfig = ShellToolConfig {
    name: "powershell",
    label: "powershell",
    shell_name: "PowerShell",
    prompt: "PS>",
    prompt_snippet: "Execute PowerShell commands",
    prompt_guidelines: &["You can inspect PI_* environment variables for current model and session details."],
    temp_file_prefix: "pi-powershell",
};

/// The `powershell` tool. Built by [`ShellTool::powershell`].
pub type PowerShellTool = ShellTool;

/// Options of the PowerShell tool (upstream `PowerShellToolOptions`).
#[derive(Clone)]
pub struct PowerShellToolOptions {
    /// Runs the commands. Default: [`LocalPowerShellOperations`].
    pub operations: Option<Arc<dyn ShellOperations>>,
    /// Expose the session's `PI_*` variables to commands. Default: true.
    pub expose_session_environment: bool,
    pub spawn_hook: Option<ShellSpawnHook>,
    /// Supplies the session variables. Sessions supply their own; standalone tools default to
    /// [`super::set_session_environment`].
    pub session_environment: Option<SessionEnvironmentFn>,
}

impl Default for PowerShellToolOptions {
    fn default() -> Self {
        Self { operations: None, expose_session_environment: true, spawn_hook: None, session_environment: None }
    }
}

/// Local PowerShell (`pwsh.exe`, then `powershell.exe`) with UTF-8 output. Fails on other systems.
#[derive(Clone)]
pub struct LocalPowerShellOperations {
    inner: LocalShellOperations,
}

impl Default for LocalPowerShellOperations {
    fn default() -> Self {
        Self { inner: LocalShellOperations::new("PowerShell", powershell_shell_config) }
    }
}

#[async_trait]
impl ShellOperations for LocalPowerShellOperations {
    async fn exec(&self, command: &str, cwd: &Path, options: ShellExecOptions) -> Result<Option<i32>, ShellExecError> {
        self.inner.exec(&format!("{UTF8_OUTPUT_PREFIX}{command}"), cwd, options).await
    }
}

impl ShellTool {
    pub fn powershell(cwd: PathBuf, options: PowerShellToolOptions) -> Self {
        let operations = options.operations.unwrap_or_else(|| Arc::new(LocalPowerShellOperations::default()));
        let mut tool = Self::with_config(cwd, POWERSHELL_TOOL_CONFIG, operations);
        tool.expose_session_environment = options.expose_session_environment;
        tool.spawn_hook = options.spawn_hook;
        tool.session_environment = options.session_environment;
        tool
    }
}
