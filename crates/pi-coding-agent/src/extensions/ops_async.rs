//! Asynchronous host operations: dialogs, model requests, compaction, nested tool calls,
//! session replacement, built-in tools, and shell commands.

use std::sync::Arc;

use futures::StreamExt;
use pi_agent::{AgentToolResult, ExecuteToolOptions, ToolUpdateCallback};
use pi_ai::{AssistantMessageEventStream, Model};
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::convert::stream_options_from_json;
use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use super::{ops_ui, replace, shell};
use crate::agent_session::auth::request_auth;
use crate::agent_session::{ModelRuntime, lock};

fn model(value: Value) -> Result<Model, JsError> {
    serde_json::from_value(value).map_err(|failure| error(format!("Invalid model: {failure}")))
}

/// A tool update callback that forwards partial results to the JavaScript callback `key`.
pub(crate) fn js_update_callback(runner: &Arc<ExtensionRunner>, key: Option<u64>) -> Option<ToolUpdateCallback> {
    let key = key?;
    let host = runner.host().clone();
    Some(Arc::new(move |partial: AgentToolResult| {
        let host = host.clone();
        let partial = json!(partial);
        tokio::spawn(async move {
            let _ = host.call_method("invokeCallback", vec![json!(key).into(), partial.into()]).await;
        });
    }))
}

/// Push each event to the JavaScript callback `key` in order; resolve with the final message.
pub(crate) async fn pump(runner: &ExtensionRunner, mut stream: AssistantMessageEventStream, key: u64) -> Value {
    let mut last = None;
    while let Some(event) = stream.next().await {
        let value = json!(event);
        if let Some(message) = value
            .get("message")
            .or_else(|| value.get("error"))
            .filter(|_| matches!(value["type"].as_str(), Some("done" | "error")))
        {
            last = Some(message.clone());
        }
        let _ = runner.host().call_method("invokeCallback", vec![json!(key).into(), value.into()]).await;
    }
    last.unwrap_or(Value::Null)
}

async fn dialog(runner: &Arc<ExtensionRunner>, name: &str, args: &Args) -> Result<Value, JsError> {
    let target = args.u64(0);
    let ui = runner.target_ui(target);
    let title = args.str(1)?;
    Ok(match name {
        "select" => {
            let options: Vec<String> = serde_json::from_value(args.json(2)).unwrap_or_default();
            json!(ui.select(&title, &options, ops_ui::dialog_options(runner, &args.json(3))).await)
        }
        "confirm" => json!(ui.confirm(&title, &args.str(2)?, ops_ui::dialog_options(runner, &args.json(3))).await),
        "input" => {
            let placeholder = args.opt_str(2);
            json!(ui.input(&title, placeholder.as_deref(), ops_ui::dialog_options(runner, &args.json(3))).await)
        }
        _ => {
            let prefill = args.opt_str(2);
            json!(ui.editor(&title, prefill.as_deref(), Default::default()).await)
        }
    })
}

async fn nested_call(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let (call_id, name, call_args, options) = (args.str(1)?, args.str(2)?, args.json(3), args.json(4));
    let context = lock(&runner.nested).get(&call_id).cloned();
    let context = context.unwrap_or_else(|| pi_agent::ToolContext::new(call_id.clone()));
    let cancel = options["signal"].as_u64().map(|id| runner.token(Some(id)));
    let on_update = js_update_callback(runner, options["updateKey"].as_u64());
    let outcome = context.execute_tool(&name, call_args, ExecuteToolOptions { cancel, on_update }).await;
    Ok(json!({"toolCall": outcome.tool_call, "result": outcome.result, "isError": outcome.is_error}))
}

async fn model_op(runner: &Arc<ExtensionRunner>, name: &str, args: Args) -> Result<Value, JsError> {
    let session = runner.target_session(args.u64(0))?;
    if name == "stream" {
        let model = model(args.json(1))?;
        let context: pi_ai::Context = serde_json::from_value(args.json(2)).map_err(error)?;
        let options_json = args.json(3);
        let mut options = stream_options_from_json(&options_json);
        options.cancel = runner.token(options_json["signal"].as_u64());
        let stream = (session.stream_fn())(&model, &context, options);
        return Ok(pump(runner, stream, args.u64(4)).await);
    }
    runtime_op(runner, session.model_runtime().clone(), name, args).await
}

/// Model requests that need only `runtime`, shared by `ctx.modelRegistry` and SDK runtimes.
pub(super) async fn runtime_op(
    runner: &Arc<ExtensionRunner>,
    runtime: Arc<ModelRuntime>,
    name: &str,
    args: Args,
) -> Result<Value, JsError> {
    Ok(match name {
        "refresh" => {
            let result = runtime.refresh_catalogs(true, None, false).await;
            json!({"aborted": result.aborted, "errors": result.errors})
        }
        "auth" => match request_auth(&runtime, &model(args.json(1))?).await {
            Ok(request) => json!({"ok": true, "apiKey": request.auth.api_key, "headers": request.auth.headers}),
            Err(message) => json!({"ok": false, "error": message}),
        },
        "apiKey" => json!(runtime.api_key_for(&args.str(1)?).await),
        "usage" => {
            let refresh = args.bool(2);
            let usages = match args.opt_str(1) {
                Some(provider) => vec![runtime.usage().get(&provider, refresh).await],
                None => runtime.usage().all(refresh).await,
            };
            json!({"providers": usages})
        }
        "providerAuth" => {
            // Upstream `getProviderAuth` is `getAuth`: ambient auth, else stored credentials (OAuth
            // refreshed), environment keys, and config keys.
            let provider = args.str(1)?;
            let resolved = match runtime.resolve_provider_auth(&provider).await {
                Some(resolved) => resolved,
                None => runtime.resolver().resolve(&provider, &Default::default()).await.map_err(|e| e.to_string()),
            };
            match resolved {
                Ok(Some(resolved)) => {
                    json!({"apiKey": resolved.auth.api_key, "headers": resolved.auth.headers, "source": resolved.source})
                }
                _ => Value::Null,
            }
        }
        "classify" => {
            let model = model(args.json(1))?;
            let context = serde_json::from_value(args.json(2)).map_err(error)?;
            let options_json = args.json(3);
            let auth = request_auth(&runtime, &model).await.map_err(error)?;
            let mut options = pi_ai::classify::ClassifierOptions::default();
            options.request.api_key = auth.auth.api_key;
            options.request.headers = auth.auth.headers;
            options.request.cancel = runner.token(options_json["signal"].as_u64());
            options.temperature = options_json["temperature"].as_f64();
            json!(runtime.classify(&auth.model, &context, options).await)
        }
        "generateImages" => {
            let model = model(args.json(1))?;
            let context = serde_json::from_value(args.json(2)).map_err(error)?;
            let options_json = args.json(3);
            let auth = request_auth(&runtime, &model).await.map_err(error)?;
            let mut options = pi_ai::images::ImagesOptions::default();
            options.request.api_key = auth.auth.api_key;
            options.request.headers = auth.auth.headers;
            options.request.cancel = runner.token(options_json["signal"].as_u64());
            json!(runtime.generate_images(&auth.model, &context, options).await)
        }
        other => return Err(error(format!("Unknown model operation: {other}"))),
    })
}

pub(crate) async fn run(runner: Arc<ExtensionRunner>, op: &str, args: Vec<HostValue>) -> Result<HostValue, JsError> {
    let args = Args(args);
    let (group, name) = op.split_once('.').unwrap_or((op, ""));
    let value = match group {
        "ui" if name == "custom" => ops_ui::custom(&runner, args.u64(0), args.u64(1), args.json(2)).await?,
        "ui" if name == "autocompleteBase" => ops_ui::autocomplete_base(&runner, &args).await?,
        "ui" if name == "externalEditor" => {
            let ui = runner.target_ui(args.u64(0));
            match ui.external_editor(&args.str(1)?, &args.str(2)?).await {
                Ok(content) => json!({"status": "complete", "content": content}),
                Err(message) => json!({"status": "failed", "message": message}),
            }
        }
        "ui" if name == "login" => {
            let ui = runner.target_ui(args.u64(0));
            json!(ui.login(&args.str(1)?).await.map_err(error)?)
        }
        "ui" => dialog(&runner, name, &args).await?,
        "models" => model_op(&runner, name, args).await?,
        "tools" if name == "executeNested" => nested_call(&runner, &args).await?,
        "tools" if name == "builtinExecute" => shell::builtin_execute(&runner, &args).await?,
        "shell" => shell::exec(&runner, &args).await?,
        "ctx" if name == "compact" => {
            let session = runner.target_session(args.u64(0))?;
            let instructions = args.opt_str(1);
            let result =
                session.compact(instructions.as_deref()).await.map_err(|failure| error(format!("{failure:#}")))?;
            crate::agent_session::compaction_result_json(&result)
        }
        "ctx" if name == "waitForIdle" => {
            runner.target_session(args.u64(0))?.wait_for_idle().await;
            Value::Null
        }
        "session" => replace::run(&runner, name, &args).await?,
        "action" => {
            let session = runner.target_session(args.u64(0))?;
            match name {
                "setModel" => {
                    let model = model(args.json(1))?;
                    if !session.model_runtime().has_configured_auth(&model.provider) {
                        json!(false)
                    } else {
                        session.set_model(model, true).await.map_err(|failure| error(format!("{failure:#}")))?;
                        json!(true)
                    }
                }
                "sendMessage" => {
                    if let Some(run) = session.send_extension_message(args.json(1), &args.json(2)).map_err(error)? {
                        run.await.map_err(|failure| error(format!("{failure:#}")))?;
                    }
                    Value::Null
                }
                "sendUserMessage" => {
                    let result = session.send_extension_user_message(args.json(1), &args.json(2)).await;
                    result.map_err(|failure| error(format!("{failure:#}")))?;
                    Value::Null
                }
                other => return Err(error(format!("Unknown action: {other}"))),
            }
        }
        "boundary" => {
            let session = runner.session().ok_or_else(|| error("No session"))?;
            session.boundary_preview(&args.str(0)?, &args.json(1)).map_err(error)?
        }
        "clipboard" => {
            let text = args.str(0)?;
            tokio::task::spawn_blocking(move || crate::clipboard::copy_to_clipboard(&text))
                .await
                .map_err(error)?
                .map_err(|failure| error(format!("{failure:#}")))?;
            Value::Null
        }
        "ai" if name == "stream" => {
            let mut model = model(args.json(1))?;
            model.api = args.str(0)?;
            let context: pi_ai::Context = serde_json::from_value(args.json(2)).map_err(error)?;
            let options_json = args.json(3);
            let mut options = stream_options_from_json(&options_json);
            options.cancel = runner.token(options_json["signal"].as_u64());
            let stream = runner.stream_api(&model, &context, options);
            pump(&runner, stream, args.u64(4)).await
        }
        "ai" => super::ops_ai::run(&runner, name, args).await?,
        "oauth" => super::provider::oauth_callback(&runner, &args).await?,
        "inline" if name == "forward" => super::ops_sdk_resources::forward(&runner, &args).await?,
        "inline" => super::inline::event(&runner, &args).await?,
        _ => return Err(error(format!("Unknown host operation: {op}"))),
    };
    Ok(HostValue::Json(value))
}
