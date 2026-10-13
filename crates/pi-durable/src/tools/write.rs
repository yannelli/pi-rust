//! The `write` tool: create or overwrite a file, creating parent directories.

use std::sync::Arc;

use pi_ai::JsonObject;
use pi_chord::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::file_mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_tool_path;
use super::{ToolCallApi, check_aborted, parse_args, require_env, thrown};
use crate::errors::DurableResult;
use crate::harness::{ToolExecutionResult, ToolRegistration, define_tool};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteToolInput {
    pub path: String,
    pub content: String,
}

pub fn create_write_tool() -> ToolRegistration {
    define_tool(
        "write",
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        }),
        |args, api, context| execute_write(args, Arc::new(api), context),
    )
}

/// The `write` tool's execution against any call api.
pub async fn execute_write(
    args: JsonObject,
    api: Arc<dyn ToolCallApi>,
    context: Context,
) -> DurableResult<ToolExecutionResult> {
    let WriteToolInput { path, content } = parse_args(args)?;
    let env = require_env(api.as_ref())?;
    let absolute = resolve_tool_path(env.as_ref(), &path, &context).await?;
    let mutate = || async {
        check_aborted(&context)?;
        env.write_file(&absolute, content.as_bytes(), &context).await.map_err(thrown)?;
        check_aborted(&context)?;
        Ok(ToolExecutionResult::text(format!("Successfully wrote to {path}")))
    };
    with_file_mutation_queue(env.as_ref(), &absolute, mutate, &context).await
}
