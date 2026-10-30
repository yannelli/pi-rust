//! Google Antigravity through Google's Agent Client Protocol server (`agy_acp_server`), which the
//! user installs and logs in to. pi runs its own tools: the agent calls them over a loopback MCP bridge.

mod bridge;
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

use self::bridge::{BridgeTool, error_result};
use self::runtime::{Runtime, rpc, slot};
use crate::providers::acp::connection::{Connection, Incoming};
use crate::providers::common::short_hash;
use crate::registry::{ApiProvider, StreamOptions};
use crate::simple_options::{clamp_thinking_level, provider_thinking_value};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, error_stream, event_channel};
use crate::types::{ContentBlock, Context, Message, Model, ModelCost, StopReason, ThinkingLevel, Usage};

pub(crate) use self::install::{install, release, root as install_root};
pub(crate) use self::process::NOT_INSTALLED;
pub(crate) use self::runtime::login;
pub(crate) use self::usage::usage;

/// Whether pi finds the Antigravity ACP server.
pub(crate) fn installed() -> bool {
    process::locate(&|name| std::env::var(name).ok()).is_some()
}
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

fn effort(model: &Model, options: &StreamOptions) -> Option<String> {
    let level = clamp_thinking_level(model, options.reasoning.unwrap_or(ThinkingLevel::Off));
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
    let shared = slot(options.session_id.as_deref().unwrap_or("default"));
    let mut guard = shared.lock().await;
    let messages = &context.messages;
    let tools = bridge_tools(context);
    let system = context.system_prompt.as_deref();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let fingerprint = short_hash(&format!("{}\u{0}{}", system.unwrap_or_default(), names.join(",")));
    if let Some(runtime) = guard.as_mut().filter(|runtime| !runtime.parked.is_empty()) {
        match resolve_parked(runtime, messages) {
            true => runtime.seen = messages.len(),
            false => *guard = None,
        }
    }
    let stale = guard.as_ref().is_some_and(|runtime| {
        runtime.prompt.is_none()
            && (runtime.fingerprint != fingerprint
                || runtime.seen > messages.len()
                || runtime.connection.closed_reason().is_some())
    });
    if stale {
        *guard = None;
    }
    if guard.is_none() {
        let cwd = std::env::current_dir().map_err(|error| format!("Cannot read the working directory: {error}"))?;
        *guard = Some(Runtime::start(cwd, system, tools, fingerprint).await?);
    }
    let runtime = guard.as_mut().expect("started above");
    if runtime.prompt.is_none() {
        runtime.select(&model.id, effort(model, options).as_deref()).await?;
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
        if let Some(runtime) = guard.as_mut() {
            runtime.connection.notify("session/cancel", json!({"sessionId": runtime.session_id}));
            resolve_parked(runtime, &[]);
        }
        *guard = None;
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
