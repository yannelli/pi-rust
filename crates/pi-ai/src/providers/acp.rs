//! ACP agents (Kiro CLI, Amp, and agents of the ACP registry) as a provider. pi starts the agent
//! over stdio and streams its session into one assistant message; the agent runs its own tools.

mod auth;
mod checkpoints;
mod client;
pub(crate) mod connection;
mod host;
mod mcp;
mod process;
mod prompt;
mod proxy;
mod render;
mod session;
mod terminal;
mod updates;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use self::auth::{AuthMethod, TerminalLogin, auth_methods, auth_methods_of, authenticate};
pub use self::host::{
    AcpHost, AgentCommand, AgentLaunch, Choice, FileChange, McpProxy, PermissionRequest, PlanEntry, PriorFile,
    ProxyTool, Selector, SessionInfo, agent, set_agents, set_host,
};
use self::render::Transcript;
use self::session::{Runtime, Shared, is_current, rpc, slot, take_slots};
use crate::registry::{ApiProvider, StreamOptions};
use crate::simple_options::{MappedLevel, mapped_thinking_level};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, event_channel};
use crate::types::{Context, Message, Model, ModelCost, StopReason, Usage};

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

/// Starts the agent session of `pi_session` without a prompt, so its models, modes, and thinking
/// levels are known before the first turn, which then reuses the session. Waits for a running start or turn.
pub async fn open_session(provider: &str, pi_session: &str) -> Result<SessionInfo, String> {
    let agent = agent(provider).ok_or_else(|| format!("No ACP agent is set up for {provider}. Run /acp."))?;
    let live = !pi_session.is_empty() && host::host().is_none_or(|host| host.live_session(pi_session));
    if !live {
        return Err(format!("{pi_session} is not a live pi session"));
    }
    open_in(&agent, pi_session, slot(&agent.provider, pi_session)).await
}

/// Opens the session in `shared`. A `close_session` while this open waited for the slot removed it,
/// so the open fails and starts no agent; a later open gets a new slot.
async fn open_in(agent: &AgentLaunch, pi_session: &str, shared: Shared) -> Result<SessionInfo, String> {
    let mut guard = shared.lock().await;
    if !is_current(&agent.provider, pi_session, &shared) {
        return Err(format!("The {} session of {pi_session} was closed", agent.name));
    }
    let stale =
        guard.as_ref().is_some_and(|runtime| runtime.connection.closed_reason().is_some() || runtime.agent != *agent);
    if stale {
        *guard = None;
    }
    if guard.is_none() {
        *guard = Some(start_live(agent, pi_session).await?);
    }
    Ok(guard.as_ref().expect("started above").info())
}

/// Stops the agents of `pi_session` with their process groups, and drops the values waiting for
/// a session; a running turn ends first. Other pi sessions keep theirs.
pub async fn close_session(pi_session: &str) {
    PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).retain(|(_, session), _| session != pi_session);
    for shared in take_slots(pi_session) {
        let runtime = shared.lock().await.take();
        if let Some(mut runtime) = runtime {
            runtime.process.shutdown().await;
        }
    }
}

/// The live session of `pi_session`: `session/load` or `session/new`, the replay dropped, the
/// values set before it started applied, and the host told.
async fn start_live(agent: &AgentLaunch, pi_session: &str) -> Result<Runtime, String> {
    let cwd = match host::host().and_then(|host| host.session_cwd(pi_session)) {
        Some(cwd) => cwd,
        None => std::env::current_dir().map_err(|error| format!("Cannot read the working directory: {error}"))?,
    };
    let (mut runtime, resumed) = Runtime::start(agent.clone(), cwd, pi_session, false).await?;
    if resumed {
        skip_replay(&mut runtime).await;
        runtime.resumed = true;
    }
    let key = (agent.provider.clone(), pi_session.to_string());
    let pending = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&key);
    for (id, value) in pending.unwrap_or_default() {
        runtime.set(&id, &value).await?;
    }
    runtime.report();
    Ok(runtime)
}

async fn run(
    out: &mut MessageBuilder,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<StopReason, String> {
    let agent =
        agent(&model.provider).ok_or_else(|| format!("No ACP agent is set up for {}. Run /acp.", model.provider))?;
    let live = options.session_id.as_deref().is_some_and(|id| host::host().is_none_or(|host| host.live_session(id)));
    let Some(pi_session) = options.session_id.as_deref().filter(|_| live) else {
        return one_shot(out, agent, model, context, options).await;
    };
    let shared = slot(&agent.provider, pi_session);
    let mut guard = shared.lock().await;
    if !session::is_current(&agent.provider, pi_session, &shared) {
        return Err(format!("The {} session of {pi_session} was closed", agent.name));
    }
    let messages = &context.messages;
    if let Some(runtime) = guard.as_mut()
        && let Some(index) = last_reply(messages, &agent.provider, runtime.last_reply)
    {
        runtime.seen = index + 1;
    }
    let stale = guard.as_ref().is_some_and(|runtime| {
        runtime.connection.closed_reason().is_some() || runtime.seen > messages.len() || runtime.agent != agent
    });
    if stale {
        *guard = None;
    }
    if guard.is_none() {
        *guard = Some(start_live(&agent, pi_session).await?);
    }
    let runtime = guard.as_mut().expect("started above");
    if std::mem::take(&mut runtime.resumed) {
        runtime.seen = messages.len().saturating_sub(1);
    }
    if let Some(proxy) = &runtime.proxy {
        proxy.refresh(pi_session).await;
    }
    prepare(runtime, model, options).await?;
    let blocks = prompt::build(messages, runtime.seen, runtime.capabilities, &runtime.client.cwd);
    out.start();
    for notice in std::mem::take(&mut runtime.notices) {
        out.text_delta(&format!("> {notice}\n\n"));
    }
    turn_hook(runtime, true).await;
    let result = drive(out, runtime, blocks, &model.cost, &options.cancel).await;
    turn_hook(runtime, false).await;
    runtime.seen = messages.len();
    runtime.last_reply = Some(out.message().timestamp);
    runtime.report();
    result
}

/// The index of the reply with `timestamp` that this agent wrote, also after compaction.
fn last_reply(messages: &[Message], provider: &str, timestamp: Option<i64>) -> Option<usize> {
    let timestamp = timestamp?;
    messages.iter().rposition(|message| {
        matches!(message, Message::Assistant(reply)
            if reply.api == API && reply.provider == provider && reply.timestamp == timestamp)
    })
}

/// Selects the model and thinking level of the request.
async fn prepare(runtime: &mut Runtime, model: &Model, options: &StreamOptions) -> Result<(), String> {
    if model.id != DEFAULT_MODEL {
        runtime.set("model", &model.id).await?;
    }
    runtime.pi_model = model.id.clone();
    let Some(thought) = runtime.selectors.iter().find(|selector| selector.category == "thought_level") else {
        return Ok(());
    };
    let Some(level) = options.reasoning else { return Ok(()) };
    let value = match mapped_thinking_level(model, level) {
        MappedLevel::Value(value) => value.to_string(),
        MappedLevel::Default => level.as_str().to_string(),
        MappedLevel::Unsupported => return Ok(()),
    };
    if thought.choices.iter().any(|choice| choice.id == value) {
        let id = thought.id.clone();
        runtime.set(&id, &value).await?;
    }
    Ok(())
}

/// [`AcpHost::turn_started`] or [`AcpHost::turn_ended`] on a blocking thread.
async fn turn_hook(runtime: &Runtime, started: bool) {
    let Some(host) = host::host().filter(|_| !runtime.client.read_only) else { return };
    let (pi_session, cwd) = (runtime.client.pi_session.clone(), runtime.client.cwd.clone());
    let _ = tokio::task::spawn_blocking(move || match started {
        true => host.turn_started(&pi_session, &cwd),
        false => host.turn_ended(&pi_session, &cwd),
    })
    .await;
}

/// A request outside a live pi session, such as a compaction summary: a new read-only session
/// of its own, whose process ends with the reply.
async fn one_shot(
    out: &mut MessageBuilder,
    agent: AgentLaunch,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<StopReason, String> {
    let cwd = std::env::current_dir().map_err(|error| format!("Cannot read the working directory: {error}"))?;
    let pi_session = options.session_id.clone().unwrap_or_default();
    let (mut runtime, _) = Runtime::start(agent, cwd, &pi_session, true).await?;
    let result = async {
        prepare(&mut runtime, model, options).await?;
        let blocks = prompt::one_shot(context, runtime.capabilities, &runtime.client.cwd);
        out.start();
        drive(out, &mut runtime, blocks, &model.cost, &options.cancel).await
    }
    .await;
    runtime.process.shutdown().await;
    result
}

/// Drops the history that `session/load` replays; pi has it already.
async fn skip_replay(runtime: &mut Runtime) {
    let connection = runtime.connection.clone();
    let mut incoming = connection.incoming.lock().await;
    while let Ok(event) = incoming.try_recv() {
        updates::apply(runtime, &event.params["update"]);
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
    runtime.client.checkpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
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
    let kind = update["sessionUpdate"].as_str().unwrap_or_default();
    if updates::apply(runtime, update) && kind != "usage_update" {
        return runtime.report();
    }
    let client = &runtime.client;
    if matches!(kind, "tool_call" | "tool_call_update") && !client.read_only {
        let mut checkpoints = client.checkpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        checkpoints.tool_update(&client.pi_session, &client.cwd, update);
    }
    transcript.update(out, update, &client.terminals);
    if kind == "plan"
        && !client.read_only
        && let Some(host) = host::host()
    {
        host.plan(&client.pi_session, &transcript.plan);
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

#[cfg(test)]
mod open_tests;
