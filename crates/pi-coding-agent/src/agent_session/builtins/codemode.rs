//! The built-in `codemode` tool (upstream `extensions/codemode`): registered inactive, with the
//! `codemode.mode` and `codemode.inlineBudget` settings read whenever the loadout changes, and the
//! codemode reference embedded in the binary for the `models` API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{
    AgentTool, AgentToolResult, ToolContext, ToolExposure, ToolLoadout, ToolLoadoutChanges, ToolUpdateCallback,
};
use pi_codemode::{CODEMODE_TOOL_NAME, CodemodeHost, CodemodeMode, CodemodeTool, CodemodeToolOptions};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::settings::SettingsManager;

/// The codemode reference: globals, tool results, `store()`, the `models` API, and limits.
pub const CODEMODE_DOCS: &str = include_str!("codemode.md");

/// Write the embedded reference to `<agent dir>/docs/codemode.md` when it differs, so the model
/// can read the path the description names. Returns the path.
pub fn codemode_docs_path(agent_dir: &Path) -> PathBuf {
    let path = agent_dir.join("docs").join(pi_codemode::CODEMODE_DOCS_FILE_NAME);
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() != Some(CODEMODE_DOCS) {
        let written = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| {
            std::fs::write(&path, CODEMODE_DOCS)?;
            Ok(())
        });
        if let Err(error) = written {
            eprintln!("Warning: could not write {}: {error}", path.display());
        }
    }
    path
}

fn mode_of(settings: &SettingsManager) -> CodemodeMode {
    match settings.codemode_mode() {
        crate::settings::schema::CodemodeMode::On => CodemodeMode::On,
        crate::settings::schema::CodemodeMode::Only => CodemodeMode::Only,
    }
}

/// The `codemode` tool of a session. Delegates to [`CodemodeTool`], is inactive by default, and
/// presents the loadout with the current settings.
pub struct SessionCodemodeTool {
    inner: CodemodeTool,
    settings: Arc<SettingsManager>,
    models: bool,
    docs_path: String,
}

impl SessionCodemodeTool {
    pub fn new(host: Arc<dyn CodemodeHost>, settings: Arc<SettingsManager>, docs_path: &Path) -> Self {
        let models = host.models().is_some();
        let docs_path = docs_path.display().to_string();
        let options = Self::options_from(&settings, models, &docs_path);
        Self { inner: CodemodeTool::new(host, options), settings, models, docs_path }
    }

    fn options_from(settings: &SettingsManager, models: bool, docs_path: &str) -> CodemodeToolOptions {
        CodemodeToolOptions {
            mode: mode_of(settings),
            inline_budget: settings.codemode_inline_budget() as usize,
            models,
            docs_path: docs_path.to_string(),
        }
    }
}

#[async_trait]
impl AgentTool for SessionCodemodeTool {
    fn name(&self) -> &str {
        CODEMODE_TOOL_NAME
    }

    fn label(&self) -> &str {
        CODEMODE_TOOL_NAME
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    fn constrained_sampling(&self) -> Option<Value> {
        self.inner.constrained_sampling()
    }

    fn exposure(&self) -> ToolExposure {
        self.inner.exposure()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        self.inner.prompt_snippet()
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        self.inner.prompt_guidelines()
    }

    /// Activate it with `--tools`, the `defaultTools` setting, or the active tool set; MCP
    /// activates it when MCP tools are only reachable from scripts.
    fn default_active(&self) -> bool {
        false
    }

    fn prepare_loadout(&self, loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        let options = Self::options_from(&self.settings, self.models, &self.docs_path);
        Some(pi_codemode::prepare_codemode_loadout(loadout, &options))
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.inner.execute(tool_call_id, args, cancel, on_update).await
    }

    async fn execute_with_context(
        &self,
        context: &ToolContext,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.inner.execute_with_context(context, args, cancel, on_update).await
    }
}
