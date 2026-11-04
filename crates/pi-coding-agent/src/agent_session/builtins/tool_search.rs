//! The built-in `tool_search` tool (upstream `extensions/tool-search`): ranks the tools that are
//! not declared to the model with BM25 and activates the matches, so the next request declares
//! them in a system message and the load survives `/tree`, resume, and fork on that branch.

use std::sync::Weak;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolExposure, ToolReach, ToolUpdateCallback};
use pi_codemode::{
    CodemodeToolInfo, ContextSearchEntry, TOOL_SEARCH_DESCRIPTION, TOOL_SEARCH_PROMPT_SNIPPET, TOOL_SEARCH_TOOL_NAME,
    format_deferred_search_result, format_tool_search_result, search_deferred, tool_search_limit, tool_search_schema,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent_session::{AgentSession, lock};
use crate::resources::context_entries::deferred_search_entries;

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
        let context = session.deferred_context();
        let found = search_deferred(&tools, &active, &context, query, limit);
        let loaded: Vec<String> = found.tools.iter().map(|tool| tool.name.clone()).collect();
        if !loaded.is_empty() {
            active.extend(loaded.iter().cloned());
            session.set_active_tools_by_name(&active);
        }
        let mut details = json!({ "loaded": loaded });
        if !found.context.is_empty() {
            let entries: Vec<Value> =
                found.context.iter().map(|entry| json!({ "kind": entry.kind, "name": entry.name })).collect();
            details["context"] = json!(entries);
        }
        Ok(AgentToolResult::text(format_deferred_search_result(&found)).with_details(details))
    }
}

impl AgentSession {
    /// Deferred skills and context entries of the current prompt options, which `tool_search` finds.
    pub(crate) fn deferred_context(&self) -> Vec<ContextSearchEntry> {
        let options = self.effective_prompt_options();
        deferred_search_entries(&options.skills, &options.context_entries, &options.context_exposure)
    }

    /// Activate the built-in `tool_search` when skills or context entries are deferred, as MCP does
    /// for deferred tools, so they stay reachable. Without it they are listed in the system prompt.
    pub(crate) fn ensure_context_discovery(&self) {
        if self.deferred_context().is_empty() {
            return;
        }
        {
            let mut tools = lock(&self.tools);
            let registered =
                tools.registry.get(TOOL_SEARCH_TOOL_NAME).is_some_and(|tool| tool.declares_reaches_tools());
            let mut active = tools.registry.active_names();
            if !registered || active.iter().any(|name| name == TOOL_SEARCH_TOOL_NAME) {
                return;
            }
            active.push(TOOL_SEARCH_TOOL_NAME.to_string());
            if tools.set_active(&active) {
                tools.pending.clear();
            }
        }
        self.sync_agent_tools();
    }
}
