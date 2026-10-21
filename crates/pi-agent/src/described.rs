//! A tool with a replaced model-facing description, as `prepare_loadout` hooks produce.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::nested::ToolContext;
use crate::tool_model::{
    ToolAnnotations, ToolExposure, ToolLoadout, ToolLoadoutChanges, ToolNamespace, ToolReach, ToolReplay,
};
use crate::types::{AgentTool, AgentToolResult, SharedTool, ToolExecutionMode, ToolUpdateCallback};

/// Delegates everything to the wrapped tool except the description.
pub struct DescribedTool {
    inner: SharedTool,
    description: String,
}

impl DescribedTool {
    pub fn inner(&self) -> &SharedTool {
        &self.inner
    }
}

/// `tool` with `description` as its model-facing description.
pub fn with_description(tool: SharedTool, description: impl Into<String>) -> SharedTool {
    Arc::new(DescribedTool { inner: tool, description: description.into() })
}

#[async_trait]
impl AgentTool for DescribedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn label(&self) -> &str {
        self.inner.label()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    fn output_schema(&self) -> Option<Value> {
        self.inner.output_schema()
    }

    fn constrained_sampling(&self) -> Option<Value> {
        self.inner.constrained_sampling()
    }

    fn exposure(&self) -> ToolExposure {
        self.inner.exposure()
    }

    fn namespace(&self) -> Option<&ToolNamespace> {
        self.inner.namespace()
    }

    fn annotations(&self) -> Option<&ToolAnnotations> {
        self.inner.annotations()
    }

    fn reaches_tools(&self, input: &Value) -> Option<ToolReach> {
        self.inner.reaches_tools(input)
    }

    fn declares_reaches_tools(&self) -> bool {
        self.inner.declares_reaches_tools()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        self.inner.prompt_snippet()
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        self.inner.prompt_guidelines()
    }

    fn default_active(&self) -> bool {
        self.inner.default_active()
    }

    fn replay(&self) -> Option<ToolReplay> {
        self.inner.replay()
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.inner.execution_mode()
    }

    fn prepare_arguments(&self, args: Value) -> anyhow::Result<Value> {
        self.inner.prepare_arguments(args)
    }

    fn prepare_loadout(&self, loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        self.inner.prepare_loadout(loadout)
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
