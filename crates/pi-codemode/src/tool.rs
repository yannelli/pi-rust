//! The `codemode` tool as an [`AgentTool`]: the model writes JavaScript that calls other tools
//! through the [`CodemodeHost`], and only the script's output reaches the model.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{
    AgentTool, AgentToolResult, ExecuteToolOptions, ToolContext, ToolLoadout, ToolLoadoutChanges, ToolUpdateCallback,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::description::{
    CODEMODE_PROMPT_GUIDELINES, CODEMODE_PROMPT_SNIPPET, CODEMODE_TOOL_NAME, CodemodeDescriptionOptions,
    create_codemode_description,
};
use crate::execute::{CodemodeToolOutput, execute_codemode};
use crate::host::{CodemodeHost, CodemodeModels, CodemodeToolInfo, NestedToolOutcome, ToolExposure};
use crate::loadout::{CodemodeToolOptions, prepare_codemode_loadout};
use crate::source::CODEMODE_SOURCE_GRAMMAR;
use crate::store::CodemodeStoreEntryData;

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

    async fn run_as_result(
        &self,
        host: Arc<dyn CodemodeHost>,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
        let output = execute_codemode(host, tool_call_id, code, cancel, Some(on_update), &self.options).await?;
        Ok(AgentToolResult {
            content: output.content,
            details: Some(serde_json::to_value(&output.details)?),
            usage: output.usage,
            is_error: output.is_error,
            ..AgentToolResult::default()
        })
    }
}

/// The session host with nested calls routed through the agent loop's tool context, so they
/// run with the loop's hooks and land in the codemode call's `nested_calls`.
struct ContextHost {
    inner: Arc<dyn CodemodeHost>,
    context: ToolContext,
}

#[async_trait]
impl CodemodeHost for ContextHost {
    fn tools(&self) -> Vec<CodemodeToolInfo> {
        self.context.tools().iter().map(|tool| CodemodeToolInfo::from_tool(tool.as_ref())).collect()
    }

    async fn execute_tool(
        &self,
        _parent: &str,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> NestedToolOutcome {
        let options = ExecuteToolOptions { cancel: Some(cancel), on_update: None };
        let outcome = self.context.execute_tool(name, args, options).await;
        NestedToolOutcome {
            tool_call_id: outcome.tool_call.id,
            content: outcome.result.content,
            structured_content: outcome.result.structured_content,
            is_error: outcome.is_error,
        }
    }

    fn store_snapshot(&self) -> Map<String, Value> {
        self.inner.store_snapshot()
    }

    fn append_store_entry(&self, data: CodemodeStoreEntryData) {
        self.inner.append_store_entry(data);
    }

    fn models(&self) -> Option<&dyn CodemodeModels> {
        self.inner.models()
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

    fn exposure(&self) -> ToolExposure {
        CODEMODE_TOOL_EXPOSURE
    }

    fn constrained_sampling(&self) -> Option<Value> {
        Some(codemode_constrained_sampling())
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(CODEMODE_PROMPT_SNIPPET)
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        CODEMODE_PROMPT_GUIDELINES.iter().map(|guideline| guideline.to_string()).collect()
    }

    fn prepare_loadout(&self, loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        let options =
            CodemodeToolOptions { models: self.options.models && self.host.models().is_some(), ..self.options.clone() };
        Some(prepare_codemode_loadout(loadout, &options))
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.run_as_result(self.host.clone(), tool_call_id, args, cancel, on_update).await
    }

    async fn execute_with_context(
        &self,
        context: &ToolContext,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let host: Arc<dyn CodemodeHost> = if context.can_execute_tools() {
            Arc::new(ContextHost { inner: self.host.clone(), context: context.clone() })
        } else {
            self.host.clone()
        };
        self.run_as_result(host, context.tool_call_id(), args, cancel, on_update).await
    }
}
