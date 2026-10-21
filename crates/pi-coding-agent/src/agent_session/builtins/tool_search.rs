//! The built-in `tool_search` tool (upstream `extensions/tool-search`): ranks the tools that are
//! not declared to the model with BM25 and activates the matches, so the next request declares
//! them in a system message and the load survives `/tree`, resume, and fork on that branch.

use std::sync::Weak;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolExposure, ToolReach, ToolUpdateCallback};
use pi_codemode::{
    CodemodeToolInfo, TOOL_SEARCH_DESCRIPTION, TOOL_SEARCH_PROMPT_SNIPPET, TOOL_SEARCH_TOOL_NAME,
    format_tool_search_result, search_deferred_tools, tool_search_limit, tool_search_schema,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent_session::AgentSession;

pub struct ToolSearchTool {
    session: Weak<AgentSession>,
}

impl ToolSearchTool {
    pub fn new(session: Weak<AgentSession>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl AgentTool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_TOOL_NAME
    }

    fn description(&self) -> &str {
        TOOL_SEARCH_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        tool_search_schema()
    }

    /// Searching changes what the model sees; scripts have no use for it.
    fn exposure(&self) -> ToolExposure {
        ToolExposure::ModelOnly
    }

    /// A query can match any tool that is not declared yet.
    fn reaches_tools(&self, _input: &Value) -> Option<ToolReach> {
        Some(ToolReach::All)
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(TOOL_SEARCH_PROMPT_SNIPPET)
    }

    fn default_active(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or_default();
        let limit = tool_search_limit(query, args.get("limit").and_then(Value::as_f64)).map_err(anyhow::Error::msg)?;
        let Some(session) = self.session.upgrade() else {
            return Ok(AgentToolResult::text(format_tool_search_result(&[])).with_details(json!({ "loaded": [] })));
        };
        let (tools, mut active) = session.with_tool_registry(|registry| {
            let tools: Vec<(CodemodeToolInfo, ToolExposure)> = registry
                .tools()
                .iter()
                .map(|tool| (CodemodeToolInfo::from_tool(tool.as_ref()), tool.exposure()))
                .collect();
            (tools, registry.active_names())
        });
        let found = search_deferred_tools(&tools, &active, query, limit);
        let loaded: Vec<String> = found.iter().map(|tool| tool.name.clone()).collect();
        if !loaded.is_empty() {
            active.extend(loaded.iter().cloned());
            session.set_active_tools_by_name(&active);
        }
        Ok(AgentToolResult::text(format_tool_search_result(&found)).with_details(json!({ "loaded": loaded })))
    }
}
