//! The `codemode` tool as an [`AgentTool`]: the model writes JavaScript that calls other tools
//! through the [`CodemodeHost`], and only the script's output reaches the model.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::Tool;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::description::{CODEMODE_TOOL_NAME, CodemodeDescriptionOptions, create_codemode_description};
use crate::execute::{CodemodeToolOutput, execute_codemode};
use crate::host::{CodemodeHost, ToolExposure};
use crate::loadout::CodemodeToolOptions;
use crate::source::CODEMODE_SOURCE_GRAMMAR;

/// Scripts must not start other scripts, so the tool is offered to the model only.
pub const CODEMODE_TOOL_EXPOSURE: ToolExposure = ToolExposure::ModelOnly;

/// JSON Schema of the `codemode` arguments.
pub fn codemode_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "code": { "type": "string", "description": "Raw JavaScript source." } },
        "required": ["code"],
    })
}

/// Capable models write the script as raw text instead of a JSON-escaped string.
pub fn codemode_constrained_sampling() -> Value {
    json!({ "type": "grammar", "variants": { "openai_lark": CODEMODE_SOURCE_GRAMMAR } })
}

pub struct CodemodeTool {
    host: Arc<dyn CodemodeHost>,
    options: CodemodeToolOptions,
    description: String,
}

impl CodemodeTool {
    /// The description lists the host's current tools within the inline budget. Sessions replace
    /// it from [`prepare_codemode_loadout`](crate::loadout::prepare_codemode_loadout) with
    /// [`with_description`](Self::with_description).
    pub fn new(host: Arc<dyn CodemodeHost>, options: CodemodeToolOptions) -> Self {
        let models = options.models && host.models().is_some();
        let description = create_codemode_description(
            &host.tools(),
            &CodemodeDescriptionOptions {
                models_docs_path: models.then(|| options.docs_path.clone()),
                deferred: HashSet::new(),
                inline_budget: Some(options.inline_budget),
            },
        );
        Self { host, options, description }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn options(&self) -> &CodemodeToolOptions {
        &self.options
    }

    /// Run a script and keep the parts [`AgentToolResult`] has no field for, such as `usage`.
    pub async fn run(
        &self,
        tool_call_id: &str,
        code: &str,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateCallback>,
    ) -> anyhow::Result<CodemodeToolOutput> {
        Ok(execute_codemode(self.host.clone(), tool_call_id, code, cancel, on_update, &self.options).await?)
    }
}

#[async_trait]
impl AgentTool for CodemodeTool {
    fn name(&self) -> &str {
        CODEMODE_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        codemode_schema()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
        let output = self.run(tool_call_id, code, cancel, Some(on_update)).await?;
        Ok(AgentToolResult {
            content: output.content,
            details: Some(serde_json::to_value(&output.details)?),
            is_error: output.is_error,
            terminate: false,
        })
    }

    fn declaration(&self) -> Tool {
        Tool {
            name: CODEMODE_TOOL_NAME.into(),
            description: self.description.clone(),
            parameters: codemode_schema(),
            constrained_sampling: Some(codemode_constrained_sampling()),
        }
    }
}
