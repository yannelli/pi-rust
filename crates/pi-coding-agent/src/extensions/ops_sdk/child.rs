//! `AgentSession` members of an SDK child session: reads, controls, prompts, and the event
//! subscription that lasts until `unsubscribe` or `dispose`.

use std::path::Path;
use std::sync::Arc;

use pi_ai::{ContentBlock, ThinkingLevel};
use pi_js::{JsError, JsRef};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::SdkState;
use crate::agent_session::{AgentSession, PromptOptions, StreamingBehavior, compaction_result_json, lock};
use crate::extensions::ops::{Args, error};
use crate::extensions::ops_session::tool_info_json;
use crate::extensions::runner::ExtensionRunner;

enum Delivery {
    Event(Value),
    /// Answered once every earlier event reached JavaScript, with the first listener error since
    /// the previous flush.
    Flush(oneshot::Sender<Option<JsError>>),
}

/// The session listener of a child and the task that passes its events to one JavaScript callback.
pub(crate) struct Subscription {
    listener: u64,
    sender: mpsc::UnboundedSender<Delivery>,
    task: JoinHandle<()>,
}

impl Subscription {
    pub(super) fn stop(self, session: Option<&AgentSession>) {
        if let Some(session) = session {
            session.unsubscribe(self.listener);
        }
        self.task.abort();
    }
}

fn subscribe(host: JsRef, sdk: &SdkState, id: u64, session: &Arc<AgentSession>, key: u64) {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let events = sender.clone();
    let listener = session.subscribe(move |event| {
        let _ = events.send(Delivery::Event(event.to_json()));
    });
    let weak = Arc::downgrade(session);
    let task = tokio::spawn(async move {
        let mut failure = None;
        while let Some(delivery) = receiver.recv().await {
            match delivery {
                Delivery::Event(event) => {
                    if let Err(error) = host.call_method("invokeCallback", vec![json!(key).into(), event.into()]).await
                    {
                        if let Some(session) = weak.upgrade() {
                            session.request_abort();
                        }
                        failure.get_or_insert(error);
                    }
                }
                Delivery::Flush(reply) => {
                    let _ = reply.send(failure.take());
                }
            }
        }
    });
    let previous = lock(&sdk.subscriptions).insert(id, Subscription { listener, sender, task });
    if let Some(previous) = previous {
        previous.stop(Some(session));
    }
}

pub(super) fn unsubscribe(sdk: &SdkState, id: u64, session: Option<&AgentSession>) {
    let removed = lock(&sdk.subscriptions).remove(&id);
    if let Some(subscription) = removed {
        subscription.stop(session);
    }
}

/// Wait until the events emitted so far reached JavaScript; fail with a listener error.
async fn flush(sdk: &SdkState, id: u64) -> Result<(), JsError> {
    let sender = lock(&sdk.subscriptions).get(&id).map(|subscription| subscription.sender.clone());
    let Some(sender) = sender else {
        return Ok(());
    };
    let (reply, answer) = oneshot::channel();
    if sender.send(Delivery::Flush(reply)).is_err() {
        return Ok(());
    }
    answer.await.ok().flatten().map_or(Ok(()), Err)
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, JsError> {
    serde_json::from_value(value).map_err(error)
}

fn images(value: &Value) -> Result<Vec<ContentBlock>, JsError> {
    if value.is_null() { Ok(Vec::new()) } else { parse(value.clone()) }
}

fn thinking_level(value: &str) -> Result<ThinkingLevel, JsError> {
    crate::model_resolver::parse_thinking_level(value).ok_or_else(|| error(format!("Invalid thinking level: {value}")))
}

/// Upstream `AgentState`; `tools` holds the active tool infos.
fn state(session: &AgentSession) -> Value {
    let (all, active, model) = (session.all_tools(), session.active_tool_names(), session.model());
    let system_prompt = session.system_prompt();
    let tools: Vec<Value> =
        active.iter().filter_map(|name| all.iter().find(|info| &info.name == name)).map(tool_info_json).collect();
    session.with_state(|state| {
        let mut pending: Vec<&String> = state.pending_tool_calls.iter().collect();
        pending.sort();
        json!({
            "systemPrompt": system_prompt,
            "model": model,
            "thinkingLevel": state.thinking_level,
            "tools": tools,
            "messages": state.messages,
            "isStreaming": state.is_streaming,
            "streamingMessage": state.streaming_message,
            "pendingToolCalls": pending,
            "errorMessage": state.error_message,
        })
    })
}

pub(super) fn read(session: &AgentSession, member: &str) -> Result<Value, JsError> {
    Ok(match member {
        "messages" => json!(session.messages()),
        "model" => json!(session.model()),
        "thinkingLevel" => json!(session.thinking_level()),
        "isStreaming" => json!(session.is_streaming()),
        "sessionId" => json!(session.session_id()),
        "sessionFile" => json!(session.session_file()),
        "systemPrompt" => json!(session.system_prompt()),
        "routedModel" => json!(
            session.routed_model().map(|routed| json!({"model": routed.model, "thinkingLevel": routed.thinking_level}))
        ),
        "contextUsage" => json!(session.context_usage().map(|usage| {
            json!({"tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent})
        })),
        "state" => state(session),
        "isIdle" => json!(session.is_idle()),
        "isCompacting" => json!(session.is_compacting()),
        "pendingMessageCount" => json!(session.pending_message_count()),
        "steeringMessages" => json!(session.steering_messages()),
        "followUpMessages" => json!(session.follow_up_messages()),
        "availableThinkingLevels" => json!(session.available_thinking_levels()),
        "supportsThinking" => json!(session.supports_thinking()),
        "steeringMode" => json!(session.steering_mode()),
        "followUpMode" => json!(session.follow_up_mode()),
        "autoCompactionEnabled" => json!(session.auto_compaction_enabled()),
        "retryAttempt" => json!(session.retry_attempt()),
        "isRetrying" => json!(session.is_retrying()),
        "autoRetryEnabled" => json!(session.auto_retry_enabled()),
        "isBashRunning" => json!(session.is_bash_running()),
        "hasPendingBashMessages" => json!(session.has_pending_bash_messages()),
        "sessionName" => json!(session.session_name()),
        "sessionStats" => session.session_stats().to_json(),
        "userMessagesForForking" => Value::Array(
            session
                .user_messages_for_forking()
                .into_iter()
                .map(|(entry_id, text)| json!({"entryId": entry_id, "text": text}))
                .collect(),
        ),
        "lastAssistantText" => json!(session.last_assistant_text()),
        "activeToolNames" => json!(session.active_tool_names()),
        "callableToolNames" => json!(session.callable_tool_names()),
        "allTools" => Value::Array(session.all_tools().iter().map(tool_info_json).collect()),
        member => return Err(JsError::unsupported(&format!("AgentSession.{member}"))),
    })
}

/// Members that return nothing; `false` when `name` is not one of them.
fn set(session: &AgentSession, name: &str, args: &Args) -> Result<bool, JsError> {
    match name {
        "setThinkingLevel" => session.set_thinking_level(thinking_level(&args.str(2)?)?, args.bool(3)),
        "setSteeringMode" => session.set_steering_mode(parse(args.json(2))?),
        "setFollowUpMode" => session.set_follow_up_mode(parse(args.json(2))?),
        "abortCompaction" => session.abort_compaction(),
        "abortBranchSummary" => session.abort_branch_summary(),
        "setAutoCompactionEnabled" => session.set_auto_compaction_enabled(args.bool(2)),
        "abortRetry" => session.abort_retry(),
        "setAutoRetryEnabled" => session.set_auto_retry_enabled(args.bool(2)),
        "setCacheWarmingMode" => session.set_cache_warming_mode(parse(args.json(2))?),
        "abortBash" => session.abort_bash(),
        "setSessionName" => session.set_session_name(&args.str(2)?).map_err(error)?,
        "refreshContext" => session.refresh_context(),
        "setActiveToolsByName" => session.set_active_tools_by_name(&parse::<Vec<String>>(args.json(2))?),
        _ => return Ok(false),
    }
    Ok(true)
}

/// Synchronous members, called as `sdk.control(id, name, ...args)`.
pub(super) fn control(runner: &ExtensionRunner, session: &Arc<AgentSession>, args: &Args) -> Result<Value, JsError> {
    let (id, name) = (args.u64(0), args.str(1)?);
    if set(session, &name, args)? {
        return Ok(Value::Null);
    }
    Ok(match name.as_str() {
        "subscribe" => {
            subscribe(runner.host().clone(), &runner.sdk, id, session, args.u64(2));
            Value::Null
        }
        "unsubscribe" => {
            unsubscribe(&runner.sdk, id, Some(session));
            Value::Null
        }
        "clearQueue" => {
            let (steering, follow_up) = session.clear_queue();
            json!({"steering": steering, "followUp": follow_up})
        }
        "cycleThinkingLevel" => json!(session.cycle_thinking_level(args.bool(2))),
        "cycleModel" => json!(session.cycle_model(args.str(2)? == "backward", args.bool(3)).map(|result| {
            json!({"model": result.model, "thinkingLevel": result.thinking_level, "isScoped": result.is_scoped})
        })),
        "exportToJsonl" => {
            let path = session.export_to_jsonl(args.opt_str(2).as_deref().map(Path::new)).map_err(error)?;
            json!(path.display().to_string())
        }
        "exportToHtml" => {
            let path = session.export_to_html(args.opt_str(2).as_deref()).map_err(error)?;
            json!(path.display().to_string())
        }
        member => return Err(JsError::unsupported(&format!("AgentSession.{member}"))),
    })
}

async fn prompt(sdk: &SdkState, session: &AgentSession, args: &Args) -> Result<Value, JsError> {
    let options = args.json(2);
    let streaming_behavior = match options["streamingBehavior"].as_str() {
        None => None,
        Some("steer") => Some(StreamingBehavior::Steer),
        Some("followUp") => Some(StreamingBehavior::FollowUp),
        Some(other) => return Err(error(format!("Invalid streamingBehavior: {other}"))),
    };
    let prompt_options = PromptOptions {
        images: images(&options["images"])?,
        expand_prompt_templates: options["expandPromptTemplates"].as_bool().unwrap_or(true),
        streaming_behavior,
        source: options["source"].as_str().map(str::to_owned),
        ..Default::default()
    };
    let _ = flush(sdk, args.u64(0)).await;
    let result = session.prompt_with(&args.str(1)?, prompt_options).await;
    let delivered = flush(sdk, args.u64(0)).await;
    result.map_err(error)?;
    delivered.map(|()| Value::Null)
}

/// Asynchronous members, called as `sdk.<name>(id, ...args)`.
pub(super) async fn run(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<Value, JsError> {
    let session = runner.sdk.session(args.u64(0))?;
    Ok(match name {
        "prompt" => prompt(&runner.sdk, &session, args).await?,
        "steer" | "followUp" => {
            let behavior = if name == "steer" { StreamingBehavior::Steer } else { StreamingBehavior::FollowUp };
            let source = args.json(3)["source"].as_str().unwrap_or("interactive").to_owned();
            let disposition = session
                .queue_user_input(&args.str(1)?, images(&args.json(2))?, behavior, &source)
                .await
                .map_err(error)?;
            json!(disposition.as_str())
        }
        "waitForIdle" => {
            session.wait_for_idle().await;
            Value::Null
        }
        "setModel" => {
            let model = serde_json::from_value::<pi_ai::Model>(args.json(1))
                .map_err(|failure| error(format!("Invalid model: {failure}")))?;
            session.set_model(model, args.bool(2)).await.map_err(error)?;
            Value::Null
        }
        "compact" => compaction_result_json(&session.compact(args.opt_str(1).as_deref()).await.map_err(error)?),
        "abort" => {
            session.abort().await;
            Value::Null
        }
        _ => return Err(JsError::unsupported(&format!("AgentSession.{name}"))),
    })
}
