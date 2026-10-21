//! The `tool_search` tool, a copy of the coding agent's (upstream `tool-search/tool-search.ts`): the model searches
//! the deferred tools it is not offered yet, and `control.add_tools` loads the matches for the next request. Loading
//! writes the conversation's `model_tools` (its `add`), so loaded tools stay offered on that branch.

use pi_ai::ContentBlock;
use pi_chord::Json;
use pi_codemode::tool_search::{
    ToolSearchDocument, ToolSearchResultTool, create_tool_search_document, format_tool_search_result, search_tools,
    tool_search_limit, tool_search_schema,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use pi_codemode::tool_search::{DEFAULT_TOOL_SEARCH_LIMIT, TOOL_SEARCH_DESCRIPTION, TOOL_SEARCH_TOOL_NAME};

use crate::errors::DurableError;
use crate::harness::{
    Extension, ToolCaller, ToolControl, ToolExecutionResult, ToolRegistration, ToolReplay, define_tool,
};

/// `tool_search` result details: the tools it loaded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSearchToolDetails {
    pub loaded: Vec<String>,
}

/// The `tool_search` tool. Only the model calls it: it changes what the model is offered, and scripts have their
/// own `searchTools()`.
pub fn create_tool_search_tool() -> ToolRegistration {
    define_tool(TOOL_SEARCH_TOOL_NAME, TOOL_SEARCH_DESCRIPTION, tool_search_schema(), |args, api, context| async move {
        let query = args.get("query").and_then(Value::as_str).unwrap_or_default();
        let limit = tool_search_limit(query, args.get("limit").and_then(Value::as_f64)).map_err(DurableError::error)?;
        let candidates = api.agent(&context).await?.deferred;
        let documents: Vec<ToolSearchDocument> = candidates
            .iter()
            .map(|tool| create_tool_search_document(tool.name(), &tool.tool.description, &tool.tool.parameters, None))
            .collect();
        let found: Vec<ToolSearchResultTool> = search_tools(query, &documents, limit)
            .into_iter()
            .map(|found| {
                let tool = candidates.iter().find(|tool| tool.name() == found.name);
                let description = tool.map(|tool| tool.tool.description.clone()).unwrap_or_default();
                ToolSearchResultTool { name: found.name, description }
            })
            .collect();
        let loaded: Vec<String> = found.iter().map(|tool| tool.name.clone()).collect();
        let details = ToolSearchToolDetails { loaded: loaded.clone() };
        let control = (!loaded.is_empty()).then(|| ToolControl { add_tools: Some(loaded), ..ToolControl::default() });
        Ok(ToolExecutionResult {
            output: Some(vec![ContentBlock::text(format_tool_search_result(&found))]),
            details: Some(Json::from(serde_json::to_value(details).unwrap_or_default())),
            control,
            ..ToolExecutionResult::default()
        })
    })
    .with_callers(vec![ToolCaller::Model])
    .with_replay(ToolReplay::Safe)
}

/// The `tool_search` extension: install it and select it where the model should find deferred tools.
pub fn create_tool_search_extension() -> Extension {
    Extension::new("tool-search").tool(create_tool_search_tool())
}
