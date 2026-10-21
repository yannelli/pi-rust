//! The host side of one execution: relays calls to tools and globals, collects output, and ends
//! the run on completion, deadline, or cancellation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::task::Poll;
use std::time::Instant;

use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{Map, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use super::types::{
    CodemodeCall, CodemodeCallStatus, CodemodeCallTarget, CodemodeDeclaredGlobal, CodemodeDeclaredTool, CodemodeDuring,
    CodemodeError, CodemodeErrorKind, CodemodeHostCall, CodemodeOutcome, CodemodeOutputItem, CodemodeResult,
    CodemodeStoreWrites, CodemodeSuccess, OnCall, OnIdle, OnStarted, SandboxFunction, Timeout,
};
use super::worker::{self, CallTarget, Deterministic, HostReply, WorkerMessage, WorkerSetup};
use crate::identifier::to_codemode_identifier;
use crate::js::{js_stringify, parse_js_json};

/// What a host-controlled run shares between the run and the handle that answers its calls.
#[derive(Default)]
pub(crate) struct RunState {
    pub finished: bool,
    /// Result numbers `deliver()` used.
    pub delivered: HashSet<u64>,
    /// Calls the script made that no result has answered yet.
    pub waiting: HashSet<u64>,
}

pub(crate) type SharedRunState = Arc<Mutex<RunState>>;

pub(crate) fn lock_state(state: &SharedRunState) -> std::sync::MutexGuard<'_, RunState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A call's answer, as `deliver()` queued it.
pub(crate) struct Delivery {
    pub seq: u64,
    pub now: f64,
    pub id: u64,
    pub outcome: CodemodeOutcome,
}

/// Callbacks of a host-controlled run: its calls go to the caller, which answers them with `deliver()`.
pub(crate) struct ControlCallbacks {
    pub on_started: Option<OnStarted>,
    pub on_call: OnCall,
    pub on_idle: Option<OnIdle>,
    pub state: SharedRunState,
}

pub(crate) struct Control {
    pub mode: Deterministic,
    pub callbacks: ControlCallbacks,
    pub deliveries: UnboundedReceiver<Delivery>,
}

pub(crate) struct ExecutionParams {
    pub code: String,
    /// Runs the script's calls; empty when `control` is set.
    pub tools: Vec<SandboxFunction>,
    pub globals: Vec<SandboxFunction>,
    /// `[{ name, jsName, description }]` and `[{ name, spread }]`: what the script sees.
    pub tools_json: Value,
    pub globals_json: Value,
    pub timeout: Timeout,
    pub cancel: Option<CancellationToken>,
    pub closed: CancellationToken,
    pub memory_limit: Option<usize>,
    pub acquire_worker: Option<super::AcquireWorker>,
    pub store: Map<String, Value>,
    pub control: Option<Control>,
}

/// A payload from the worker that does not decode. The execution fails as a sandbox error.
#[derive(Debug, PartialEq)]
pub(crate) struct BridgeError(pub String);

fn parse_bridge_json(json: &str, what: &str) -> Result<Value, BridgeError> {
    parse_js_json(json).map_err(|_| BridgeError(format!("{what} is not valid JSON")))
}

pub(crate) fn parse_store_writes(json: &str) -> Result<CodemodeStoreWrites, BridgeError> {
    let Value::Array(entries) = parse_bridge_json(json, "store writes")? else {
        return Err(BridgeError("store writes are not an array".into()));
    };
    let mut writes = CodemodeStoreWrites::default();
    for entry in entries {
        let malformed = || BridgeError("store writes contain a malformed entry".into());
        let Value::Array(parts) = entry else { return Err(malformed()) };
        match parts.as_slice() {
            [Value::String(key)] => writes.delete.push(key.clone()),
            [Value::String(key), Value::String(json)] => {
                let value = parse_bridge_json(json, &format!("store value for {}", crate::js::quote(key)))?;
                writes.set.insert(key.clone(), value);
            }
            _ => return Err(malformed()),
        }
    }
    Ok(writes)
}

pub(crate) fn parse_script_error(json: &str) -> Result<CodemodeError, BridgeError> {
    let Value::Object(fields) = parse_bridge_json(json, "script error")? else {
        return Err(BridgeError("script error is not an object".into()));
    };
    let optional = |key: &str| match fields.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(BridgeError("script error is malformed".into())),
    };
    let Some(Value::String(message)) = fields.get("message") else {
        return Err(BridgeError("script error is malformed".into()));
    };
    Ok(CodemodeError {
        kind: CodemodeErrorKind::Script,
        name: optional("name")?,
        message: message.clone(),
        stack: optional("stack")?,
    })
}

struct PendingCall {
    record: Option<usize>,
    started: Instant,
    token: CancellationToken,
}

type CallFuture = BoxFuture<'static, (u64, Result<Option<Value>, String>)>;

struct Execution {
    tools: HashMap<String, SandboxFunction>,
    globals: HashMap<String, SandboxFunction>,
    output: Vec<CodemodeOutputItem>,
    calls: Vec<CodemodeCall>,
    pending: HashMap<u64, PendingCall>,
    replies: Option<mpsc::Sender<HostReply>>,
    /// Set when the script ended by itself in a host-controlled run.
    during: Option<CodemodeDuring>,
}

/// The result of handling one message: `Some` ends the execution.
type Step = Option<Result<CodemodeSuccess, CodemodeError>>;

fn bridge_failure(error: BridgeError) -> Step {
    let message = format!(
        "Sandbox bridge broken: {}. The script may have modified built-ins such as a prototype's toJSON.",
        error.0
    );
    Some(Err(CodemodeError::new(CodemodeErrorKind::Sandbox, message)))
}

impl Execution {
    fn start_call(
        &mut self,
        id: u64,
        target: CallTarget,
        name: String,
        args: Option<String>,
    ) -> Result<CallFuture, BridgeError> {
        if self.pending.contains_key(&id) {
            return Err(BridgeError(format!("duplicate call id {id}")));
        }
        let is_tool = target == CallTarget::Tool;
        let record = is_tool.then(|| {
            self.calls.push(CodemodeCall {
                name: name.clone(),
                status: CodemodeCallStatus::Cancelled,
                duration_ms: 0.0,
            });
            self.calls.len() - 1
        });
        let token = CancellationToken::new();
        self.pending.insert(id, PendingCall { record, started: Instant::now(), token: token.clone() });
        let function = if is_tool { self.tools.get(&name) } else { self.globals.get(&name) }.cloned();
        Ok(Box::pin(async move {
            let Some(function) = function else {
                return (id, Err(format!("Unknown {} \"{name}\"", if is_tool { "tool" } else { "global" })));
            };
            let args = match args.map(|json| parse_js_json(&json)).transpose() {
                Ok(args) => args,
                Err(error) => return (id, Err(error.to_string())),
            };
            (id, (function.execute)(args, token).await)
        }))
    }

    fn complete_call(&mut self, id: u64, result: Result<Option<Value>, String>) {
        let reply = match result {
            Ok(value) => HostReply { id, ok: true, payload: value.as_ref().map(js_stringify), ..HostReply::default() },
            Err(message) => HostReply { id, ok: false, payload: Some(message), ..HostReply::default() },
        };
        self.finish_call(reply);
    }

    /// Record the call's status and pass its reply to the worker.
    fn finish_call(&mut self, reply: HostReply) {
        // Already cancelled by finish(): the record keeps "cancelled".
        let Some(pending) = self.pending.remove(&reply.id) else { return };
        if let Some(index) = pending.record {
            let call = &mut self.calls[index];
            call.status = if reply.ok { CodemodeCallStatus::Ok } else { CodemodeCallStatus::Error };
            call.duration_ms = pending.started.elapsed().as_secs_f64() * 1000.0;
        }
        if let Some(replies) = &self.replies {
            let _ = replies.send(reply);
        }
    }

    /// A host-controlled run: record the call and hand it to the caller, who answers with `deliver()`.
    fn forward_call(
        &mut self,
        control: &ControlCallbacks,
        id: u64,
        target: CallTarget,
        name: String,
        args: Option<String>,
    ) -> Result<(), BridgeError> {
        if self.pending.contains_key(&id) {
            return Err(BridgeError(format!("duplicate call id {id}")));
        }
        let args = args.map(|json| parse_bridge_json(&json, "call arguments")).transpose()?;
        let is_tool = target == CallTarget::Tool;
        let record = is_tool.then(|| {
            self.calls.push(CodemodeCall {
                name: name.clone(),
                status: CodemodeCallStatus::Cancelled,
                duration_ms: 0.0,
            });
            self.calls.len() - 1
        });
        self.pending.insert(id, PendingCall { record, started: Instant::now(), token: CancellationToken::new() });
        lock_state(&control.state).waiting.insert(id);
        let target = if is_tool { CodemodeCallTarget::Tool } else { CodemodeCallTarget::Global };
        (control.on_call)(CodemodeHostCall { id, target, name, args });
        Ok(())
    }

    /// A result the caller delivered with `deliver()`.
    fn answer(&mut self, delivery: Delivery) {
        let Delivery { seq, now, id, outcome } = delivery;
        let reply = match outcome {
            CodemodeOutcome::Ok(value) => HostReply {
                id,
                ok: true,
                payload: value.as_ref().map(js_stringify),
                seq: Some(seq),
                now: Some(now),
                properties: None,
            },
            CodemodeOutcome::Err { message, properties } => HostReply {
                id,
                ok: false,
                payload: Some(message),
                seq: Some(seq),
                now: Some(now),
                properties: properties.map(|properties| js_stringify(&Value::Object(properties))),
            },
        };
        self.finish_call(reply);
    }

    fn handle_done(ok: bool, value: Option<String>, writes: Option<String>, error: Option<String>) -> Step {
        let decoded = if ok {
            let value = value.map(|json| parse_bridge_json(&json, "return value")).transpose();
            let writes = parse_store_writes(writes.as_deref().unwrap_or("null"));
            match (value, writes) {
                (Ok(value), Ok(store_writes)) => Ok(Ok(CodemodeSuccess { value, store_writes })),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        } else {
            parse_script_error(error.as_deref().unwrap_or("null")).map(Err)
        };
        match decoded {
            Ok(outcome) => Some(outcome),
            Err(error) => bridge_failure(error),
        }
    }

    fn cancel_pending(&mut self) {
        for pending in std::mem::take(&mut self.pending).into_values() {
            if let Some(index) = pending.record {
                self.calls[index].duration_ms = pending.started.elapsed().as_secs_f64() * 1000.0;
            }
            pending.token.cancel();
        }
    }
}

/// Poll every call future once, so the synchronous part of a call runs in message order.
async fn poll_ready(running: &mut FuturesUnordered<CallFuture>) -> Vec<(u64, Result<Option<Value>, String>)> {
    std::future::poll_fn(|cx| {
        let mut ready = Vec::new();
        while let Poll::Ready(Some(output)) = running.poll_next_unpin(cx) {
            ready.push(output);
        }
        Poll::Ready(ready)
    })
    .await
}

fn function_table(functions: Vec<SandboxFunction>) -> HashMap<String, SandboxFunction> {
    functions.into_iter().map(|function| (function.name().to_string(), function)).collect()
}

/// `(tools, globals)` as the prelude reads them.
pub(crate) fn declarations_json(tools: &[CodemodeDeclaredTool], globals: &[CodemodeDeclaredGlobal]) -> (Value, Value) {
    let tools = tools.iter().map(|tool| {
        serde_json::json!({
            "name": tool.name,
            "jsName": tool.js_name.clone().unwrap_or_else(|| to_codemode_identifier(&tool.name)),
            "description": tool.description.clone().unwrap_or_default(),
        })
    });
    let globals = globals.iter().map(|global| serde_json::json!({ "name": global.name, "spread": global.spread }));
    (Value::Array(tools.collect()), Value::Array(globals.collect()))
}

/// Stops the worker when the execution ends, also when the caller drops the execution future.
struct InterruptOnDrop(Arc<AtomicBool>);

impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub(crate) async fn run(params: ExecutionParams) -> CodemodeResult {
    let store_json: Map<String, Value> =
        params.store.iter().map(|(key, value)| (key.clone(), Value::String(js_stringify(value)))).collect();

    let interrupt = Arc::new(AtomicBool::new(false));
    let _stop_worker = InterruptOnDrop(interrupt.clone());
    let (to_host, mut messages) = tokio::sync::mpsc::unbounded_channel();
    let (replies, replies_rx) = mpsc::channel();
    let mut execution = Execution {
        tools: function_table(params.tools),
        globals: function_table(params.globals),
        output: Vec::new(),
        calls: Vec::new(),
        pending: HashMap::new(),
        replies: Some(replies),
        during: None,
    };
    let (mode, control) = match params.control {
        Some(Control { mode, callbacks, deliveries }) => (Some(mode), Some((callbacks, deliveries))),
        None => (None, None),
    };
    let finished = control.as_ref().map(|(callbacks, _)| callbacks.state.clone());
    let setup = WorkerSetup {
        code: params.code,
        tools_json: params.tools_json.to_string(),
        globals_json: params.globals_json.to_string(),
        store_json: Value::Object(store_json).to_string(),
        memory_limit: params.memory_limit,
        interrupt: interrupt.clone(),
        deterministic: mode,
        to_host,
        replies: replies_rx,
    };

    let cancel = params.cancel.unwrap_or_default();
    let outcome = if cancel.is_cancelled() {
        Err(CodemodeError::new(CodemodeErrorKind::Aborted, "Execution aborted"))
    } else {
        let release = params.acquire_worker.as_ref().map(|acquire| (acquire.0)()).transpose();
        match release.map(|release| worker::spawn(setup, release)) {
            Err(denied) => Err(CodemodeError::new(CodemodeErrorKind::Sandbox, denied)),
            Ok(Err(error)) => {
                Err(CodemodeError::new(CodemodeErrorKind::Sandbox, format!("Failed to start worker: {error}")))
            }
            Ok(Ok(())) => drive(&mut execution, &mut messages, params.timeout, &cancel, &params.closed, control).await,
        }
    };

    // The worker stops at its next interrupt check and frees its VM on its own thread, so the
    // result does not wait for a large heap to be released.
    if let Some(state) = finished {
        lock_state(&state).finished = true;
    }
    execution.cancel_pending();
    execution.replies = None;
    drop(messages);
    CodemodeResult { outcome, output: execution.output, calls: execution.calls, during: execution.during }
}

async fn drive(
    execution: &mut Execution,
    messages: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerMessage>,
    timeout: Timeout,
    cancel: &CancellationToken,
    closed: &CancellationToken,
    control: Option<(ControlCallbacks, UnboundedReceiver<Delivery>)>,
) -> Result<CodemodeSuccess, CodemodeError> {
    let deadline = match timeout {
        Timeout::After(duration) => Some((tokio::time::Instant::now() + duration, duration.as_millis())),
        Timeout::Never => None,
    };
    let (callbacks, mut deliveries) = match control {
        Some((callbacks, deliveries)) => (Some(callbacks), Some(deliveries)),
        None => (None, None),
    };
    let mut running: FuturesUnordered<CallFuture> = FuturesUnordered::new();
    loop {
        let step: Step = tokio::select! {
            biased;
            _ = closed.cancelled() => Some(Err(CodemodeError::new(CodemodeErrorKind::Aborted, "Sandbox closed"))),
            _ = cancel.cancelled() => Some(Err(CodemodeError::new(CodemodeErrorKind::Aborted, "Execution aborted"))),
            _ = async { tokio::time::sleep_until(deadline.map(|(at, _)| at).unwrap_or_else(tokio::time::Instant::now)).await }, if deadline.is_some() => {
                let ms = deadline.map(|(_, ms)| ms).unwrap_or_default();
                Some(Err(CodemodeError::new(CodemodeErrorKind::Timeout, format!("Execution timed out after {ms} ms"))))
            }
            message = messages.recv() => match message {
                None => Some(Err(CodemodeError::new(CodemodeErrorKind::Sandbox, "Worker exited before the script settled"))),
                Some(WorkerMessage::Output(item)) => {
                    execution.output.push(item);
                    None
                }
                Some(WorkerMessage::Call { id, target, name, args }) => match &callbacks {
                    Some(callbacks) => match execution.forward_call(callbacks, id, target, name, args) {
                        Err(error) => bridge_failure(error),
                        Ok(()) => None,
                    },
                    None => match execution.start_call(id, target, name, args) {
                        Err(error) => bridge_failure(error),
                        Ok(future) => {
                            running.push(future);
                            for (id, result) in poll_ready(&mut running).await {
                                execution.complete_call(id, result);
                            }
                            None
                        }
                    },
                },
                Some(WorkerMessage::Started(fingerprint)) => {
                    if let Some(on_started) = callbacks.as_ref().and_then(|callbacks| callbacks.on_started.as_ref()) {
                        on_started(&fingerprint);
                    }
                    None
                }
                Some(WorkerMessage::Idle { seq, issued }) => {
                    if let Some(on_idle) = callbacks.as_ref().and_then(|callbacks| callbacks.on_idle.as_ref()) {
                        on_idle(seq, issued);
                    }
                    None
                }
                Some(WorkerMessage::Done { ok, value, writes, error, during }) => {
                    execution.during = during;
                    Execution::handle_done(ok, value, writes, error)
                }
                Some(WorkerMessage::Crash(message)) => Some(Err(CodemodeError::new(CodemodeErrorKind::Sandbox, message))),
            },
            delivery = async {
                match deliveries.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            }, if deliveries.is_some() => {
                match delivery {
                    Some(delivery) => execution.answer(delivery),
                    None => deliveries = None,
                }
                None
            }
            Some((id, result)) = running.next(), if !running.is_empty() => {
                execution.complete_call(id, result);
                None
            }
        };
        if let Some(outcome) = step {
            return outcome;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_bridge_payloads() {
        let failure = |json: &str| parse_store_writes(json).unwrap_err().0;
        assert_eq!(failure("null"), "store writes are not an array");
        assert_eq!(failure("[1]"), "store writes contain a malformed entry");
        assert_eq!(failure(r#"[["k", "{"]]"#), "store value for \"k\" is not valid JSON");
        assert_eq!(parse_bridge_json("{", "return value").unwrap_err().0, "return value is not valid JSON");
        assert_eq!(parse_script_error("5").unwrap_err().0, "script error is not an object");
        assert_eq!(parse_script_error("{}").unwrap_err().0, "script error is malformed");
        let writes = parse_store_writes(r#"[["a", "1"], ["b"]]"#).unwrap();
        assert_eq!(writes.set.get("a"), Some(&Value::from(1)));
        assert_eq!(writes.delete, vec!["b".to_string()]);
        let message = match bridge_failure(BridgeError("x".into())) {
            Some(Err(error)) => error.message,
            _ => String::new(),
        };
        assert!(message.starts_with("Sandbox bridge broken: x."));
    }
}
