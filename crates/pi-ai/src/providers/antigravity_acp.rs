//! Google Antigravity through Google's Agent Client Protocol server (`agy_acp_server`), which the
//! user installs and logs in to. pi runs its own tools: the agent calls them over a loopback MCP bridge.

mod bridge;
mod controls;
#[cfg(test)]
mod controls_tests;
mod install;
mod process;
pub(crate) mod prompt;
mod runtime;
#[cfg(test)]
mod tests;
mod usage;

use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) use self::bridge::{BridgeTool, error_result};
use self::controls::{ensure, is_current, slot};
use self::runtime::{Runtime, rpc};
use crate::providers::acp;
use crate::providers::acp::connection::{Connection, Incoming};
use crate::providers::common::short_hash;
use crate::registry::{ApiProvider, StreamOptions};
use crate::simple_options::{clamp_thinking_level, provider_thinking_value};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, error_stream, event_channel};
use crate::types::{ContentBlock, Context, Message, Model, ModelCost, StopReason, ThinkingLevel, Usage};

pub(crate) use self::bridge::{Bridge, ParkedCall};
pub(crate) use self::install::{install, release, root as install_root};
pub(crate) use self::process::NOT_INSTALLED;
pub(crate) use self::runtime::{OfferedModel, login, offered_models, passthrough};
pub(crate) use self::usage::usage;

/// Whether pi finds the Antigravity ACP server.
pub(crate) fn installed() -> bool {
    process::locate(&|name| std::env::var(name).ok()).is_some()
}
pub use self::controls::{close_session, open_session, session_info, set_session_option};
pub use self::runtime::{PassthroughMcpServer, set_passthrough_mcp_servers};

pub const API: &str = "antigravity-acp";
pub(crate) const LABEL: &str = "Antigravity ACP server";
/// A prompt runs as long as the agent works; pi's cancel ends it sooner.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
/// Tool calls that arrive this soon after another one join the same pi turn.
const TOOL_BATCH: Duration = Duration::from_millis(100);
const ABORTED: &str = "Request was aborted";

pub struct AntigravityAcp;

impl ApiProvider for AntigravityAcp {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        if options.api_key.as_deref().is_none_or(str::is_empty) {
            return error_stream(model, StopReason::Error, "Antigravity needs a login. Run /login google-antigravity.");
        }
        let (sender, stream) = event_channel();
        let (model, context) = (model.clone(), context.clone());
        tokio::spawn(async move {
            let mut out = MessageBuilder::new(&model, sender);
            match run(&mut out, &model, &context, &options).await {
                Ok(reason) => out.finish(reason),
                Err(_) if options.cancel.is_cancelled() => out.fail(StopReason::Aborted, ABORTED),
                Err(error) => out.fail(StopReason::Error, error),
            }
        });
        stream
    }
}

/// pi's tools for the bridge, without the ones of MCP servers that Antigravity reaches directly.
fn bridge_tools(context: &Context) -> Vec<BridgeTool> {
    let prefixes: Vec<String> = runtime::passthrough().into_iter().map(|server| server.tool_prefix).collect();
    let tools =
        context.tools.iter().filter(|tool| !prefixes.iter().any(|prefix| tool.name.starts_with(prefix.as_str())));
    tools
        .map(|tool| BridgeTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            schema: tool.parameters.clone(),
        })
        .collect()
}

/// What a session is built for: its system prompt and bridge tools; a different fingerprint needs a new session.
struct Shape {
    system: Option<String>,
    tools: Vec<BridgeTool>,
    fingerprint: String,
}

/// The shape of a provider request context, the same for a turn and for a session opened before one.
fn session_shape(context: &Context) -> Shape {
    let tools = bridge_tools(context);
    let system = context.system_prompt.clone();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let fingerprint = short_hash(&format!("{}\u{0}{}", system.as_deref().unwrap_or_default(), names.join(",")));
    Shape { system, tools, fingerprint }
}

fn level(value: &str) -> Option<ThinkingLevel> {
    match value {
        "extra_high" | "extra-high" => Some(ThinkingLevel::Xhigh),
        other => other.parse().ok(),
    }
}

/// `model` with the thinking levels of the server's `thought_level` values; other fields stay.
pub(crate) fn with_levels(mut model: Model, values: &[String]) -> Model {
    let levels: Vec<(ThinkingLevel, &str)> =
        values.iter().filter_map(|value| Some((level(value)?, value.as_str()))).collect();
    model.reasoning = !levels.is_empty();
    model.thinking_level_map = model.reasoning.then(|| {
        ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                let value = levels.iter().find(|(known, _)| *known == level).map(|(_, value)| value.to_string());
                (level.as_str().to_string(), value)
            })
            .collect()
    });
    model
}

/// `model` with the thinking levels that its open session offers now (none without a thinking selector), or
/// `None` when they are the catalog's or the session reports another model.
pub fn live_model(model: &Model, info: &acp::SessionInfo) -> Option<Model> {
    if model.api != API || info.pi_model != model.id {
        return None;
    }
    let thought = info.selectors.iter().find(|selector| selector.category == "thought_level");
    let values: Vec<String> = thought.into_iter().flat_map(|thought| &thought.choices).map(|c| c.id.clone()).collect();
    let live = with_levels(model.clone(), &values);
    (live != *model).then_some(live)
}

fn effort_for(model: &Model, level: ThinkingLevel) -> Option<String> {
    let level = clamp_thinking_level(model, level);
    (model.reasoning && level != ThinkingLevel::Off).then(|| provider_thinking_value(model, level)).flatten()
}

/// The MCP result for a pi tool result.
fn mcp_result(content: &[ContentBlock], is_error: bool) -> Value {
    let items = content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
        ContentBlock::Image { data, mime_type, .. } => {
            Some(json!({"type": "image", "data": data, "mimeType": mime_type}))
        }
        _ => None,
    });
    json!({"content": items.collect::<Vec<_>>(), "isError": is_error})
}

/// Answers each parked call with its tool result from `messages`. A missing result fails every
/// parked call and returns false, and the session is dropped.
fn resolve_parked(runtime: &mut Runtime, messages: &[Message]) -> bool {
    let result = |id: &str| {
        messages.iter().rev().find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == id => {
                Some(mcp_result(&result.content, result.is_error))
            }
            _ => None,
        })
    };
    let complete = runtime.parked.iter().all(|call| result(&call.id).is_some());
    for call in runtime.parked.drain(..) {
        let reply = if complete { result(&call.id) } else { None };
        let _ = call.reply.send(reply.unwrap_or_else(|| error_result("pi did not return this tool result")));
    }
    complete
}

async fn run(
    out: &mut MessageBuilder,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<StopReason, String> {
    let pi_session = options.session_id.as_deref().unwrap_or("default");
    let shared = slot(pi_session);
    let mut state = shared.lock().await;
    if !is_current(pi_session, &shared) {
        return Err(controls::closed(pi_session));
    }
    let messages = &context.messages;
    let mut unresolved = false;
    if let Some(runtime) = state.runtime.as_mut().filter(|runtime| !runtime.parked.is_empty()) {
        match resolve_parked(runtime, messages) {
            true => runtime.seen = messages.len(),
            false => unresolved = true,
        }
    }
    if unresolved {
        if let Some(old) = state.runtime.take() {
            old.shutdown().await;
        }
    }
    ensure(&mut state, session_shape(context), Some(messages.len())).await?;
    let runtime = state.runtime.as_mut().expect("ensured above");
    if runtime.prompt.is_none() {
        runtime.select_confirmed(model, options.reasoning.unwrap_or(ThinkingLevel::Off)).await?;
        let system = context.system_prompt.as_deref();
        let blocks = match runtime.seen {
            0 => prompt::fresh(messages, system.filter(|_| !runtime.system_in_meta), &runtime.capabilities),
            seen => prompt::delta(messages, seen, &runtime.capabilities),
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (connection, params) =
            (runtime.connection.clone(), json!({"sessionId": runtime.session_id, "prompt": blocks}));
        tokio::spawn(async move {
            let _ = sender.send(connection.request("session/prompt", params, PROMPT_TIMEOUT).await);
        });
        runtime.prompt = Some(receiver);
    }
    out.start();
    let result = drive(out, runtime, messages.len(), &model.cost, &options.cancel).await;
    if result.is_err() {
        if let Some(mut runtime) = state.runtime.take() {
            runtime.connection.notify("session/cancel", json!({"sessionId": runtime.session_id}));
            resolve_parked(&mut runtime, &[]);
            runtime.shutdown().await;
        }
    }
    result
}

async fn drive(
    out: &mut MessageBuilder,
    runtime: &mut Runtime,
    total: usize,
    cost: &ModelCost,
    cancel: &CancellationToken,
) -> Result<StopReason, String> {
    let connection = runtime.connection.clone();
    let mut incoming = connection.incoming.lock().await;
    let mut batch_end: Option<Instant> = None;
    let Some(mut prompt) = runtime.prompt.take() else { return Err("Antigravity ACP has no prompt in flight".into()) };
    loop {
        let batch = async {
            match batch_end {
                Some(end) => tokio::time::sleep_until(end).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ABORTED.into()),
            event = incoming.recv() => match event {
                Some(event) => handle(out, runtime, &connection, event),
                None => return Err(connection.closed_reason().unwrap_or_else(|| "Antigravity ACP server exited".into())),
            },
            Some(call) = runtime.calls.recv() => {
                let handle = out.tool_call_start(call.id.clone(), call.tool.clone());
                out.tool_call_arguments(handle, call.arguments.clone());
                out.end_tool_call(handle);
                runtime.parked.push(call);
                batch_end = Some(Instant::now() + TOOL_BATCH);
            }
            () = batch => {
                runtime.prompt = Some(prompt);
                return Ok(StopReason::ToolUse);
            }
            reply = &mut prompt => {
                let reply = reply.map_err(|_| "Antigravity ACP prompt ended".to_string())?.map_err(rpc)?;
                runtime.seen = total;
                return finish(out, runtime, &reply, cost);
            }
        }
    }
}

fn finish(out: &mut MessageBuilder, runtime: &Runtime, reply: &Value, cost: &ModelCost) -> Result<StopReason, String> {
    let count = |value: &Value| value.as_u64().unwrap_or_default();
    let (usage, quota) = (&reply["usage"], reply.pointer("/_meta/quota/token_count"));
    let input = quota.map_or_else(|| count(&usage["inputTokens"]), |quota| count(&quota["input_tokens"]));
    let output = quota.map_or_else(|| count(&usage["outputTokens"]), |quota| count(&quota["output_tokens"]));
    let cache_read = count(&usage["cachedReadTokens"]);
    let mut totals = Usage {
        input: input.saturating_sub(cache_read),
        output,
        cache_read,
        cache_write: count(&usage["cachedWriteTokens"]),
        reasoning: usage["thoughtTokens"].as_u64(),
        total_tokens: count(&usage["totalTokens"]),
        ..Usage::default()
    };
    if let Some(used) = runtime.context_tokens {
        totals.total_tokens = used;
    }
    totals.apply_cost(cost);
    out.message_mut().usage = totals;
    match reply["stopReason"].as_str() {
        Some("cancelled") => Err(ABORTED.into()),
        Some("max_tokens" | "max_turn_requests") => Ok(StopReason::Length),
        _ => Ok(StopReason::Stop),
    }
}

/// A `session/update` notification or an agent request.
fn handle(out: &mut MessageBuilder, runtime: &mut Runtime, connection: &Connection, event: Incoming) {
    match (event.method.as_str(), event.id) {
        ("session/update", _) => {
            let update = &event.params["update"];
            let text = update.pointer("/content/text").and_then(Value::as_str).unwrap_or_default();
            match update["sessionUpdate"].as_str().unwrap_or_default() {
                "agent_message_chunk" => out.text_delta(text),
                "agent_thought_chunk" => out.thinking_delta(text),
                "tool_call" => {
                    let title = update["title"].as_str().unwrap_or("tool");
                    if !title.contains("pi_") && !title.contains(bridge::BRIDGE_NAME) {
                        out.thinking_delta(&format!("\n[Antigravity tool: {title}]\n"));
                    }
                }
                "config_option_update" => {
                    runtime.update_config(update["configOptions"].as_array().map_or(&[][..], Vec::as_slice))
                }
                "usage_update" => runtime.context_tokens = update["used"].as_u64().filter(|used| *used > 0),
                _ => {}
            }
        }
        ("session/request_permission", Some(id)) => {
            let options = event.params["options"].as_array().cloned().unwrap_or_default();
            let pick = ["allow_once", "allow_always"]
                .iter()
                .find_map(|kind| options.iter().find(|option| option["kind"] == *kind));
            match pick.and_then(|option| option["optionId"].as_str()) {
                Some(option) => connection.respond(id, json!({"outcome": {"outcome": "selected", "optionId": option}})),
                None => connection.respond(id, json!({"outcome": {"outcome": "cancelled"}})),
            }
        }
        (_, Some(id)) => connection.respond_error(id, -32601, "Method not supported by pi"),
        _ => {}
    }
}
