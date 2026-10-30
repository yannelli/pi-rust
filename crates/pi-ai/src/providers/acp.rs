//! ACP agents (Kiro CLI, Amp, and agents of the ACP registry) as a provider. pi starts the agent
//! over stdio and streams its session into one assistant message; the agent runs its own tools.

mod auth;
mod client;
pub(crate) mod connection;
mod host;
mod process;
mod prompt;
mod render;
mod session;
mod terminal;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use self::auth::{AuthMethod, TerminalLogin, auth_methods, auth_methods_of, authenticate};
pub use self::host::{
    AcpHost, AgentLaunch, Choice, PermissionRequest, PlanEntry, Selector, SessionInfo, agent, set_agents, set_host,
};
use self::render::Transcript;
use self::session::{Runtime, rpc, slot};
use crate::registry::{ApiProvider, StreamOptions};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, event_channel};
use crate::types::{Context, Model, ModelCost, StopReason, Usage};

pub const API: &str = "acp";
/// The model ID that keeps the model the agent chose.
pub const DEFAULT_MODEL: &str = "default";
const PROMPT_TIMEOUT: Duration = Duration::from_secs(12 * 3600);
/// How long pi waits for the agent to end a cancelled turn.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
const ABORTED: &str = "Request was aborted";

/// Selector values per `(provider, pi session)` that wait for the session to start.
type Pending = HashMap<(String, String), Vec<(String, String)>>;
static PENDING: LazyLock<Mutex<Pending>> = LazyLock::new(Mutex::default);

pub struct Acp;

impl ApiProvider for Acp {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
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

/// The selectors of the live session of `provider` in `pi_session`. `None` when no session runs
/// or a turn holds it.
pub fn session_info(provider: &str, pi_session: &str) -> Option<SessionInfo> {
    let shared = slot(provider, pi_session);
    let guard = shared.try_lock().ok()?;
    guard.as_ref().map(Runtime::info)
}

/// Sets a mode or another selector. Without a live session, the value applies when one starts.
pub async fn set_session_option(provider: &str, pi_session: &str, id: &str, value: &str) -> Result<(), String> {
    let shared = slot(provider, pi_session);
    let mut guard = shared.try_lock().map_err(|_| "Wait until the agent turn ends".to_string())?;
    match guard.as_mut() {
        Some(runtime) => runtime.set(id, value).await,
        None => {
            let mut pending = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let values = pending.entry((provider.to_string(), pi_session.to_string())).or_default();
            values.retain(|(known, _)| known != id);
            values.push((id.to_string(), value.to_string()));
            Ok(())
        }
    }
}

fn thinking_value(options: &StreamOptions) -> Option<String> {
    let level = options.reasoning?;
    serde_json::to_value(level).ok()?.as_str().map(str::to_string)
}

async fn run(
    out: &mut MessageBuilder,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<StopReason, String> {
    let agent =
        agent(&model.provider).ok_or_else(|| format!("No ACP agent is set up for {}. Run /acp.", model.provider))?;
    let pi_session = options.session_id.as_deref().unwrap_or("default");
    let shared = slot(&agent.provider, pi_session);
    let mut guard = shared.lock().await;
    let messages = &context.messages;
    let stale = guard.as_ref().is_some_and(|runtime| {
        runtime.connection.closed_reason().is_some() || runtime.seen > messages.len() || runtime.agent != agent
    });
    if stale {
        *guard = None;
    }
    if guard.is_none() {
        let cwd = match host::host().and_then(|host| host.session_cwd(pi_session)) {
            Some(cwd) => cwd,
            None => std::env::current_dir().map_err(|error| format!("Cannot read the working directory: {error}"))?,
        };
        let (mut runtime, resumed) = Runtime::start(agent.clone(), cwd, pi_session).await?;
        if resumed {
            skip_replay(&mut runtime).await;
            runtime.seen = messages.len().saturating_sub(1);
        }
        let key = (agent.provider.clone(), pi_session.to_string());
        let pending = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&key);
        for (id, value) in pending.unwrap_or_default() {
            runtime.set(&id, &value).await?;
        }
        runtime.report();
        *guard = Some(runtime);
    }
    let runtime = guard.as_mut().expect("started above");
    if model.id != DEFAULT_MODEL {
        runtime.set("model", &model.id).await?;
    }
    let thought = runtime.selectors.iter().find(|selector| selector.category == "thought_level");
    let level =
        thinking_value(options).filter(|level| thought.is_some_and(|s| s.choices.iter().any(|c| &c.id == level)));
    if let (Some(level), Some(id)) = (level, thought.map(|selector| selector.id.clone())) {
        runtime.set(&id, &level).await?;
    }
    let blocks = prompt::build(messages, runtime.seen, runtime.capabilities, &runtime.client.cwd);
    out.start();
    let result = drive(out, runtime, blocks, &model.cost, &options.cancel).await;
    runtime.seen = messages.len();
    result
}

/// Drops the history that `session/load` replays; pi has it already.
async fn skip_replay(runtime: &mut Runtime) {
    let connection = runtime.connection.clone();
    let mut incoming = connection.incoming.lock().await;
    while let Ok(event) = incoming.try_recv() {
        let update = &event.params["update"];
        match update["sessionUpdate"].as_str() {
            Some("config_option_update") => {
                runtime.apply_config(update["configOptions"].as_array().map_or(&[][..], Vec::as_slice))
            }
            Some("current_mode_update") => {
                runtime.set_current("mode", update["currentModeId"].as_str().unwrap_or_default())
            }
            _ => {}
        }
    }
}

async fn drive(
    out: &mut MessageBuilder,
    runtime: &mut Runtime,
    blocks: Vec<Value>,
    cost: &ModelCost,
    cancel: &CancellationToken,
) -> Result<StopReason, String> {
    let connection = runtime.connection.clone();
    let mut incoming = connection.incoming.lock().await;
    let turn = CancellationToken::new();
    let _close_dialogs = turn.clone().drop_guard();
    let params = json!({"sessionId": runtime.session_id, "prompt": blocks});
    let prompt = connection.request("session/prompt", params, PROMPT_TIMEOUT);
    let mut prompt = std::pin::pin!(prompt);
    let mut transcript = Transcript::default();
    let mut deadline: Option<Instant> = None;
    loop {
        let grace = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = cancel.cancelled(), if deadline.is_none() => {
                turn.cancel();
                connection.notify("session/cancel", json!({"sessionId": runtime.session_id}));
                deadline = Some(Instant::now() + CANCEL_GRACE);
            }
            () = grace => return Err(ABORTED.into()),
            event = incoming.recv() => match event {
                Some(event) if event.id.is_some() => client::handle(&connection, &runtime.client, event, turn.clone()),
                Some(event) => update(out, runtime, &mut transcript, &event.params),
                None => {
                    let reason = connection.closed_reason().unwrap_or_else(|| format!("{} exited", runtime.agent.name));
                    let stderr = runtime.process.stderr_tail();
                    return Err(if stderr.is_empty() { reason } else { format!("{reason}\n{stderr}") });
                }
            },
            reply = &mut prompt => {
                transcript.flush_plan(out);
                let reply = reply.map_err(|error| rpc(&runtime.agent.name, error))?;
                let previous = runtime.cost_usd;
                runtime.cost_usd = transcript.cost_usd.or(previous);
                return finish(out, &transcript, &reply, cost, previous);
            }
        }
    }
}

fn update(out: &mut MessageBuilder, runtime: &mut Runtime, transcript: &mut Transcript, params: &Value) {
    if params["sessionId"].as_str().is_some_and(|id| id != runtime.session_id) {
        return;
    }
    let update = &params["update"];
    match update["sessionUpdate"].as_str().unwrap_or_default() {
        "config_option_update" => {
            runtime.apply_config(update["configOptions"].as_array().map_or(&[][..], Vec::as_slice));
            runtime.report();
        }
        "current_mode_update" => {
            let mode = runtime.selectors.iter().find(|selector| selector.category == "mode").map(|s| s.id.clone());
            runtime.set_current(&mode.unwrap_or_default(), update["currentModeId"].as_str().unwrap_or_default());
            runtime.report();
        }
        "plan" => {
            transcript.update(out, update, &runtime.client.terminals);
            if let Some(host) = host::host() {
                host.plan(&runtime.client.pi_session, &transcript.plan);
            }
        }
        _ => transcript.update(out, update, &runtime.client.terminals),
    }
}

/// `previous` is the session cost before the turn; `usage_update` reports the session total.
fn finish(
    out: &mut MessageBuilder,
    transcript: &Transcript,
    reply: &Value,
    cost: &ModelCost,
    previous: Option<f64>,
) -> Result<StopReason, String> {
    let count = |key: &str| reply["usage"][key].as_u64().unwrap_or_default();
    let cache_read = count("cachedReadTokens");
    let mut usage = Usage {
        input: count("inputTokens").saturating_sub(cache_read),
        output: count("outputTokens"),
        cache_read,
        cache_write: count("cachedWriteTokens"),
        reasoning: reply["usage"]["thoughtTokens"].as_u64(),
        total_tokens: transcript.context_tokens.unwrap_or_else(|| count("totalTokens")),
        ..Usage::default()
    };
    usage.apply_cost(cost);
    if let Some(amount) = transcript.cost_usd {
        usage.cost.total = (amount - previous.unwrap_or_default()).max(0.0);
        usage.cost.total_from_provider = Some(true);
    }
    out.message_mut().usage = usage;
    match reply["stopReason"].as_str() {
        Some("cancelled") => Err(ABORTED.into()),
        Some("refusal") => Err("The agent refused the request".into()),
        Some("max_tokens" | "max_turn_requests") => Ok(StopReason::Length),
        _ => Ok(StopReason::Stop),
    }
}
