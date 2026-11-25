//! `builtins.*`: the native side of `createCodemodeExtension`, `createToolSearchExtension`, and
//! `createMcpExtension` (`js/builtin_extensions.js`).

use std::sync::Arc;

use pi_agent::{AgentToolResult, ToolContext, ToolUpdateCallback};
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::ops::{Args, error};
use super::ops_async::js_update_callback;
use super::runner::ExtensionRunner;
use crate::agent_session::{factory_definition, lock};

pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    let value = match name {
        "definition" => {
            let (tool, options) = (args.str(0)?, args.json(1));
            let runtime = runner.model_runtime().clone();
            factory_definition(runtime, runner.agent_dir(), &tool, &options)
                .ok_or_else(|| error(format!("Unknown built-in tool: {tool}")))?
        }
        other => return Err(error(format!("Unknown host operation: builtins.{other}"))),
    };
    Ok(HostValue::Json(value))
}

pub(crate) async fn run(runner: Arc<ExtensionRunner>, name: &str, args: Args) -> Result<HostValue, JsError> {
    let value = match name {
        "execute" => execute(&runner, &args).await?,
        "startMcp" => {
            runner.target_session(args.u64(0))?.start_factory_mcp(&args.json(1));
            Value::Null
        }
        other => return Err(error(format!("Unknown host operation: builtins.{other}"))),
    };
    Ok(HostValue::Json(value))
}

/// Run the native tool of a factory definition for the session of `ctx`. Nested calls of a script
/// go through the context of the calling tool call, as upstream's `ctx.executeTool`.
async fn execute(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let session = runner.target_session(args.u64(0))?;
    let tool = args.str(1)?;
    let native = session
        .factory_tool(&tool, &args.json(2), true)
        .ok_or_else(|| error(format!("Unknown built-in tool: {tool}")))?;
    let (call_id, params, options) = (args.str(3)?, args.json(4), args.json(5));
    let context = lock(&runner.nested).get(&call_id).cloned().unwrap_or_else(|| ToolContext::new(&call_id));
    let cancel = runner.token(options["signal"].as_u64());
    let on_update: ToolUpdateCallback =
        js_update_callback(runner, options["updateKey"].as_u64()).unwrap_or_else(|| Arc::new(|_: AgentToolResult| {}));
    let result = native.execute_with_context(&context, params, cancel, on_update).await;
    Ok(json!(result.map_err(|failure| error(format!("{failure:#}")))?))
}
