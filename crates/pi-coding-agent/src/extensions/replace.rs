//! Session control from commands (upstream `ExtensionCommandContextActions`): new sessions,
//! forks, switching, tree navigation, and reload. A replacement session becomes a target id so
//! `withSession` and `setup` callbacks reach it from the old runtime.

use std::sync::Arc;

use futures::FutureExt;
use pi_js::JsError;
use serde_json::{Value, json};

use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use crate::agent_session::{AgentSession, AgentSessionRuntime, ForkPosition, NavigateTreeOptions, lock};

/// Make `session` reachable as a target of `runner`.
pub(crate) fn add_target(runner: &ExtensionRunner, session: &Arc<AgentSession>) -> u64 {
    let id = runner.next_id();
    lock(&runner.targets).insert(id, Arc::downgrade(session));
    id
}

fn failure(error: anyhow::Error) -> JsError {
    JsError::new(format!("{error:#}"))
}

fn runtime(runner: &ExtensionRunner, target: u64) -> Option<Arc<AgentSessionRuntime>> {
    runner.target_runner(target).bindings().runtime.and_then(|runtime| runtime.upgrade())
}

fn replaced(runner: &ExtensionRunner, runtime: &AgentSessionRuntime, cancelled: bool) -> Value {
    if cancelled {
        return json!({"cancelled": true});
    }
    json!({"cancelled": false, "target": add_target(runner, &runtime.session())})
}

pub(crate) async fn run(runner: &Arc<ExtensionRunner>, name: &str, args: &Args) -> Result<Value, JsError> {
    let target = args.u64(0);
    if name == "navigateTree" {
        let session = runner.target_session(target)?;
        let options = args.json(2);
        let options = NavigateTreeOptions {
            summarize: options["summarize"].as_bool().unwrap_or(false),
            custom_instructions: options["customInstructions"].as_str().map(str::to_string),
            replace_instructions: options["replaceInstructions"].as_bool().unwrap_or(false),
            label: options["label"].as_str().map(str::to_string),
        };
        let result = session.navigate_tree(&args.str(1)?, options).await.map_err(failure)?;
        return Ok(json!({"cancelled": result.cancelled}));
    }
    if name == "reload" {
        if runner.target_runner(target).parent.get().is_some() {
            return Err(JsError::unsupported("ctx.reload() in an SDK child session"));
        }
        let session = runner.target_session(target)?;
        session.reload_extensions().await.map_err(failure)?;
        return Ok(Value::Null);
    }
    let Some(runtime) = runtime(runner, target) else {
        return Ok(json!({"cancelled": false}));
    };
    match name {
        "new" => {
            let options = args.json(1);
            let parent = options["parentSession"].as_str().map(str::to_string);
            let setup = options["setup"].as_u64().map(|key| {
                let weak = Arc::downgrade(runner);
                let setup: crate::agent_session::AsyncSetupFn = Box::new(move |session: Arc<AgentSession>| {
                    async move {
                        let Some(runner) = weak.upgrade() else { return Ok(()) };
                        let id = add_target(&runner, &session);
                        let result = runner.call_json("invokeCallback", vec![json!(key), json!(id)]).await;
                        session.refresh_context();
                        // The JavaScript stack stays in the error, so the crash hint can name the extension.
                        result.map(drop).map_err(|error| {
                            let stack = error.stack.clone();
                            anyhow::Error::new(crate::diagnostics::StackError { message: error.message.clone(), stack })
                        })
                    }
                    .boxed()
                });
                setup
            });
            let result = runtime.new_session_with(parent, setup).await.map_err(|error| {
                let stack = crate::diagnostics::error_stack(&error);
                runner.target_ui(target).fatal_runtime_error("Failed to create session", &format!("{error:#}"), &stack);
                failure(error)
            })?;
            Ok(replaced(runner, &runtime, result.cancelled))
        }
        "fork" => {
            let options = args.json(2);
            let position = match options["position"].as_str() {
                Some("at") => ForkPosition::At,
                _ => ForkPosition::Before,
            };
            let result = runtime.fork(&args.str(1)?, position).await.map_err(|error| {
                let stack = crate::diagnostics::error_stack(&error);
                runner.target_ui(target).fatal_runtime_error("Failed to fork session", &format!("{error:#}"), &stack);
                failure(error)
            })?;
            Ok(replaced(runner, &runtime, result.cancelled))
        }
        "switch" => {
            let path = std::path::PathBuf::from(args.str(1)?);
            let result = runtime.switch_session(&path, None).await.map_err(failure)?;
            Ok(replaced(runner, &runtime, result.cancelled))
        }
        other => Err(error(format!("Unknown session operation: {other}"))),
    }
}

/// `setup` callbacks append to the new session through these (upstream `SessionManager`).
pub(crate) fn write(runner: &ExtensionRunner, args: &Args) -> Result<Value, JsError> {
    let session = runner.target_session(args.u64(0))?;
    let method = args.str(1)?;
    let mut manager = session.session_manager();
    let id = match method.as_str() {
        "appendMessage" => {
            let message: pi_ai::Message = serde_json::from_value(args.json(2)).map_err(error)?;
            manager.append_message(&message)
        }
        "appendCustomEntry" => {
            manager.append_custom_entry(&args.str(2)?, Some(args.json(3)).filter(|data| !data.is_null()))
        }
        "appendCustomMessageEntry" => {
            let content = serde_json::from_value(args.json(3)).map_err(error)?;
            let details = Some(args.json(5)).filter(|details| !details.is_null());
            manager.append_custom_message_entry(&args.str(2)?, content, args.bool(4), details)
        }
        "appendSessionInfo" => manager.append_session_info(&args.str(2)?),
        "appendLabelChange" => manager.append_label_change(&args.str(2)?, args.opt_str(3).as_deref()),
        "appendModelChange" => manager.append_model_change(&args.str(2)?, &args.str(3)?),
        "appendThinkingLevelChange" => {
            let level = super::convert::thinking_level(&args.json(2)).ok_or_else(|| error("Invalid thinking level"))?;
            manager.append_thinking_level_change(level)
        }
        other => return Err(error(format!("Unknown session method: {other}"))),
    };
    id.map(Value::from).map_err(failure)
}
