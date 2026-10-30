//! Synchronous context and action operations (upstream `ExtensionContextActions` and
//! `ExtensionActions`): reads of the session, its tree, and its models, plus the actions that
//! finish at once. Actions that run the agent continue in the background.

use std::sync::Arc;

use pi_agent::{AgentToolResult, ToolReach};
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::convert::prompt_options_from_json;
use super::ops::{Args, error};
use super::runner::{ExtensionError, ExtensionRunner};
use crate::agent_session::{AgentSession, lock};
use crate::session::SessionManager;

fn entry_json(entry: &crate::session::Entry) -> Value {
    serde_json::to_value(entry.raw()).unwrap_or(Value::Null)
}

fn tree_json(manager: &SessionManager) -> Value {
    let tree = manager.get_tree();
    fn node(tree: &crate::session::SessionTree<'_>, index: usize) -> Value {
        let current = tree.node(index);
        let children: Vec<Value> = current.children.iter().map(|child| node(tree, *child)).collect();
        let mut value = json!({"entry": entry_json(current.entry), "children": children});
        if let Some(label) = &current.label {
            value["label"] = json!(label);
        }
        value
    }
    tree.roots.iter().map(|root| node(&tree, *root)).collect()
}

fn read_session(session: &AgentSession, method: &str, args: &Args) -> Result<Value, JsError> {
    read_manager(&session.session_manager(), method, args)
}

/// `ctx.sessionManager` methods (upstream `ReadonlySessionManager`); method arguments start at 2.
pub(super) fn read_manager(manager: &SessionManager, method: &str, args: &Args) -> Result<Value, JsError> {
    Ok(match method {
        "getCwd" => json!(manager.cwd().display().to_string()),
        "getSessionDir" => json!(manager.session_dir().display().to_string()),
        "getSessionId" => json!(manager.id()),
        "getSessionFile" => json!(manager.file().map(|path| path.display().to_string())),
        "getLeafId" => json!(manager.leaf_id()),
        "getLeafEntry" => manager.leaf_entry().map(entry_json).unwrap_or(Value::Null),
        "getEntry" => manager.get_entry(&args.str(2)?).map(entry_json).unwrap_or(Value::Null),
        "getLabel" => json!(manager.get_label(&args.str(2)?)),
        "getBranch" => {
            let from = args.opt_str(2);
            manager.get_branch(from.as_deref()).into_iter().map(entry_json).collect()
        }
        "buildContextEntries" => manager.build_context_entries().into_iter().map(entry_json).collect(),
        "buildSessionProjection" => {
            let projection = manager.build_session_projection();
            let entries: Vec<Value> = projection
                .entries
                .iter()
                .map(|entry| json!({"sourceEntry": entry_json(entry.source), "messages": entry.messages}))
                .collect();
            json!({"entries": entries, "messages": projection.messages})
        }
        "buildSessionContext" => {
            let context = manager.build_session_context();
            let model = context.model.map(|(provider, model_id)| json!({"provider": provider, "modelId": model_id}));
            let level = context.thinking_level.map_or("off", |level| level.as_str());
            json!({"messages": context.messages, "model": model, "thinkingLevel": level})
        }
        "getHeader" => serde_json::to_value(manager.header_raw()).unwrap_or(Value::Null),
        "getEntries" => manager.entries().iter().map(entry_json).collect(),
        "getTree" => tree_json(manager),
        "getSessionName" => json!(manager.session_name()),
        other => return Err(error(format!("Unknown session method: {other}"))),
    })
}

/// `ctx.modelRegistry` reads.
fn read_models(session: &AgentSession, method: &str, args: &Args) -> Result<Value, JsError> {
    let runtime = session.model_runtime();
    let of_type = |model: &pi_ai::Model, kind: &str| model.model_type.as_deref().unwrap_or("chat") == kind;
    Ok(match method {
        "getError" => json!(runtime.load_error()),
        "getAll" => json!(runtime.all_models()),
        "getAvailable" => json!(runtime.available_models()),
        "find" => json!(runtime.get_model(&args.str(2)?, &args.str(3)?)),
        "findOfType" => {
            let (kind, provider, id) = (args.str(2)?, args.str(3)?, args.str(4)?);
            let found = runtime
                .all_models()
                .into_iter()
                .find(|model| model.provider == provider && model.id == id && of_type(model, &kind));
            json!(found)
        }
        "getModelsOfType" | "getAvailableOfType" => {
            let (kind, provider) = (args.str(2)?, args.opt_str(3));
            let models = if method == "getModelsOfType" { runtime.all_models() } else { runtime.available_models() };
            let models: Vec<_> = models
                .into_iter()
                .filter(|model| {
                    of_type(model, &kind) && provider.as_ref().is_none_or(|wanted| &model.provider == wanted)
                })
                .collect();
            json!(models)
        }
        "hasConfiguredAuth" => json!(runtime.has_configured_auth(&args.str(2)?)),
        "isUsingOAuth" => {
            let (provider, store) = (args.str(2)?, runtime.credential_store());
            let stored = block_on_fresh(async move { store.read(&provider).await });
            json!(matches!(stored, Some(Ok(Some(pi_ai::auth::Credential::OAuth(_))))))
        }
        "getProviderDisplayName" => {
            let provider = args.str(2)?;
            json!(runtime.registry().provider_name(&provider).map(str::to_string).unwrap_or(provider))
        }
        "getProviderAuthStatus" => {
            let provider = args.str(2)?;
            json!({"configured": runtime.has_configured_auth(&provider)})
        }
        "getProvider" => {
            let provider = args.str(2)?;
            let registry = runtime.registry();
            match registry.has_provider(&provider) {
                true => json!({"id": provider, "name": registry.provider_name(&provider).unwrap_or(&provider)}),
                false => Value::Null,
            }
        }
        "getRegisteredProviderIds" => {
            json!(session.extension_runner().map(|runner| runner.provider_ids()).unwrap_or_default())
        }
        other => return Err(error(format!("Unknown model registry method: {other}"))),
    })
}

fn tool_infos(session: &AgentSession, names: &[String]) -> Value {
    let infos = session.all_tools();
    names.iter().filter_map(|name| infos.iter().find(|info| &info.name == name)).map(tool_info_json).collect()
}

pub(crate) fn tool_info_json(info: &crate::agent_session::ToolInfo) -> Value {
    let mut value = json!({
        "name": info.name,
        "description": info.description,
        "parameters": info.parameters,
        "exposure": info.exposure,
        "sourceInfo": {"path": info.source_path, "source": info.source},
    });
    if !info.prompt_guidelines.is_empty() {
        value["promptGuidelines"] = json!(info.prompt_guidelines);
    }
    if let Some(namespace) = &info.namespace {
        value["namespace"] = json!(namespace);
    }
    value
}

/// A `getAllTools` entry. `reachesTools` is a function upstream; `api.js` turns the `true` here
/// into one.
fn all_tools_entry(info: &crate::agent_session::ToolInfo) -> Value {
    let mut value = tool_info_json(info);
    if info.reaches_tools {
        value["reachesTools"] = json!(true);
    }
    value
}

/// What `reachesTools` of a tool without extension JavaScript returns: `"all"`, the words, or null.
fn tool_reaches(session: &AgentSession, name: &str, input: &Value) -> Value {
    match session.tool(name).and_then(|tool| tool.reaches_tools(input)) {
        Some(ToolReach::All) => json!("all"),
        Some(ToolReach::Words(words)) => json!(words),
        None => Value::Null,
    }
}

fn report(runner: &ExtensionRunner, event: &str) -> impl Fn(anyhow::Error) + Send + 'static {
    let weak = Arc::downgrade(&runner.this());
    let event = event.to_string();
    move |failure| {
        if let Some(runner) = weak.upgrade() {
            let error = format!("{failure:#}");
            runner.emit_error(ExtensionError {
                extension_path: "<runtime>".into(),
                event: event.clone(),
                error,
                stack: None,
            });
        }
    }
}

/// `pi.*` actions that finish synchronously, and the ones that start background work.
fn action(runner: &ExtensionRunner, session: &Arc<AgentSession>, name: &str, args: &Args) -> Result<Value, JsError> {
    let done = Value::Null;
    match name {
        "sendMessage" => {
            let (message, options) = (args.json(1), args.json(2));
            let on_error = report(runner, "send_message");
            if let Some(run) = session.send_extension_message(message, &options).map_err(error)? {
                let busy = session.begin_extension_prompt();
                tokio::spawn(async move {
                    let result = run.await;
                    drop(busy);
                    if let Err(failure) = result {
                        on_error(failure);
                    }
                });
            }
            Ok(done)
        }
        "sendUserMessage" => {
            let (content, options) = (args.json(1), args.json(2));
            let on_error = report(runner, "send_user_message");
            let session = session.clone();
            let busy = session.begin_extension_prompt();
            tokio::spawn(async move {
                let result = session.send_extension_user_message(content, &options).await;
                drop(busy);
                if let Err(failure) = result {
                    on_error(failure);
                }
            });
            Ok(done)
        }
        "appendEntry" => {
            let data = Some(args.json(2)).filter(|data| !data.is_null());
            session.append_custom_entry(&args.str(1)?, data);
            Ok(done)
        }
        "setSessionName" => session.set_session_name(&args.str(1)?).map(|_| done).map_err(error),
        "getSessionName" => Ok(json!(session.session_name())),
        "setLabel" => session.set_label(&args.str(1)?, args.opt_str(2).as_deref()).map(|_| done).map_err(error),
        "getActiveTools" => Ok(json!(session.active_tool_names())),
        "getAllTools" => Ok(session.all_tools().iter().map(all_tools_entry).collect()),
        "toolReaches" => Ok(tool_reaches(session, &args.str(1)?, &args.json(2))),
        "getSettings" => Ok(serde_json::to_value(session.settings().settings()).unwrap_or(Value::Null)),
        "setActiveTools" => {
            let names: Vec<String> = serde_json::from_value(args.json(1)).unwrap_or_default();
            session.set_active_tools_by_name(&names);
            Ok(done)
        }
        "getCommands" => Ok(session.extension_command_infos()),
        "getThinkingLevel" => Ok(json!(session.thinking_level().as_str())),
        "setThinkingLevel" => {
            let level = crate::model_resolver::parse_thinking_level(&args.str(1)?)
                .ok_or_else(|| error("Invalid thinking level"))?;
            session.set_thinking_level(level, true);
            Ok(done)
        }
        other => Err(error(format!("Unknown action: {other}"))),
    }
}

fn context(runner: &ExtensionRunner, session: &Arc<AgentSession>, name: &str, target: u64) -> Result<Value, JsError> {
    let runner = runner.target_runner(target);
    Ok(match name {
        "mode" => json!(runner.mode().as_str()),
        "hasUI" => json!(runner.mode().has_ui()),
        "cwd" => json!(session.cwd().display().to_string()),
        "model" => json!(session.model()),
        "scopedModels" => session
            .scoped_models()
            .iter()
            .map(|scoped| {
                let mut value = json!({"model": scoped.model});
                if let Some(level) = scoped.thinking_level {
                    value["thinkingLevel"] = json!(level.as_str());
                }
                value
            })
            .collect(),
        "thinkingLevel" => json!(session.thinking_level().as_str()),
        "isIdle" => json!(session.is_idle()),
        "isProjectTrusted" => json!(session.settings().is_project_trusted()),
        "signal" => match session.agent().signal() {
            Some(token) => json!(runner.signal_for(&token)),
            None => Value::Null,
        },
        "abort" => {
            match runner.bindings().abort {
                Some(abort) => abort(),
                None => session.request_abort(),
            }
            Value::Null
        }
        "hasPendingMessages" => json!(session.pending_message_count() > 0),
        "shutdown" => {
            if let Some(shutdown) = runner.bindings().shutdown {
                shutdown();
            }
            Value::Null
        }
        "contextUsage" => match session.context_usage() {
            Some(usage) => {
                json!({"tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent})
            }
            None => Value::Null,
        },
        "systemPrompt" => json!(session.system_prompt()),
        "systemPromptOptions" => session.extension_prompt_options(),
        other => return Err(error(format!("Unknown context property: {other}"))),
    })
}

pub(crate) fn sync(runner: &ExtensionRunner, op: &str, args: &Args) -> Result<HostValue, JsError> {
    let (group, name) = op.split_once('.').unwrap_or((op, ""));
    let value = match (group, name) {
        ("tool", "update") => {
            let callback = lock(&runner.updates).get(&args.u64(0)).cloned();
            if let (Some(callback), Ok(partial)) = (callback, serde_json::from_value::<AgentToolResult>(args.json(1))) {
                callback(partial);
            }
            Value::Null
        }
        ("session", "write") => super::replace::write(runner, args)?,
        // `project_trust` handlers run before a session exists.
        ("ctx", "mode") if args.u64(0) == 0 => json!(runner.mode().as_str()),
        ("ctx", "hasUI") if args.u64(0) == 0 => json!(runner.mode().has_ui()),
        ("ctx", "cwd") if args.u64(0) == 0 && runner.session().is_none() => json!(runner.cwd().display().to_string()),
        ("prompt", "build") => {
            let options = prompt_options_from_json(&args.json(0), runner.cwd());
            json!(crate::system_prompt::render_system_prompt(&options).map_err(error)?)
        }
        _ => {
            let target = args.u64(0);
            let session = runner.target_session(target)?;
            match group {
                "ctx" => context(runner, &session, name, target)?,
                "session" => read_session(&session, &args.str(1)?, args)?,
                "models" => read_models(&session, &args.str(1)?, args)?,
                "tools" if name == "callable" => tool_infos(&session, &session.callable_tool_names()),
                "action" => action(runner, &session, name, args)?,
                _ => return Err(error(format!("Unknown host operation: {op}"))),
            }
        }
    };
    Ok(HostValue::Json(value))
}

/// Run `future` to completion on a private runtime, for synchronous reads from the JavaScript
/// thread; `None` when no runtime could start.
pub(crate) fn block_on_fresh<F: std::future::Future + Send>(future: F) -> Option<F::Output>
where
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
                Some(runtime.block_on(future))
            })
            .join()
            .ok()
            .flatten()
    })
}
