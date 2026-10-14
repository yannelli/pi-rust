//! The Session worker process loop (upstream `runSessionWorkerWithHarness` in `session-worker.ts`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use pi_chord::services::parse_service_call;
use pi_chord::state::StateListener;
use pi_chord::{
    AbortController, BACKGROUND_CONTEXT, BoxFuture, ChordError, ChordResult, Json, TODO_CONTEXT, with_cancel,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::control::{LineReader, LineWriter, connect, signals};
use super::coordinator::COORDINATOR_PROTOCOL_VERSION;
use super::lockfile::{LockOptions, LockRelease, lock as lock_file};
use super::process::lock;
use super::services::worker::create_session_worker_services;
use super::services::worker::{SessionWorkerRuntime, SessionWorkerServices, SessionWorkerServicesOptions};
use super::session_catalog::session_storage_path;
use super::session_worker::{
    SESSION_WORKER_CONTROL_ADDRESS_ENV, SESSION_WORKER_CONTROL_TOKEN_ENV, SESSION_WORKER_INITIAL_DEMAND_GRACE_ENV,
    SESSION_WORKER_ORPHAN_DEMAND_GRACE_ENV, SESSION_WORKER_PEER_ID_ENV, SESSION_WORKER_SESSION_KEY_ENV,
    SessionWorkerCommand, SessionWorkerEvent, SessionWorkerOptions, WorkerLifecycle, WorkerLifecycleOptions,
    WorkerOperationResponse, WorkerOperationScope, lifecycle_delay,
};

const DEFAULT_INITIAL_DEMAND_GRACE: Duration = Duration::from_secs(10);
const DEFAULT_ORPHAN_DEMAND_GRACE: Duration = Duration::from_secs(30);

/// Open the Session's Harness over `database_path` with its root conversation. Called while the worker
/// holds the Session lock; on failure it closes whatever it opened.
pub type CreateSessionWorkerHarness =
    Arc<dyn Fn(PathBuf, SessionWorkerOptions) -> BoxFuture<ChordResult<SessionWorkerRuntime>> + Send + Sync>;

static DEFAULT_HARNESS: OnceLock<CreateSessionWorkerHarness> = OnceLock::new();

/// Install the coding agent Harness factory the `session-worker` role uses.
pub fn set_default_session_worker_harness(create: CreateSessionWorkerHarness) {
    let _ = DEFAULT_HARNESS.set(create);
}

pub async fn run_session_worker_process(args: &[String]) -> ChordResult<()> {
    // TODO(step 2): upstream's createCodingAgentHarness needs durable/harness-setup; step 2 installs it here.
    let create = DEFAULT_HARNESS.get().cloned().unwrap_or_else(|| {
        Arc::new(|_, _| Box::pin(async { Err(ChordError::error("The coding agent Session worker is not ported yet")) }))
    });
    let result = run_session_worker_with_harness(args, create).await;
    if result.is_err() {
        std::process::exit(1);
    }
    result
}

struct Control {
    initial_server_connection_id: Option<String>,
    reader: LineReader,
    writer: LineWriter,
}

static FAILURE_WRITER: OnceLock<LineWriter> = OnceLock::new();

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn session_key() -> Option<String> {
    let encoded = env(SESSION_WORKER_SESSION_KEY_ENV)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded.trim_end_matches('=')).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

async fn connect_control() -> ChordResult<Control> {
    let (Some(address), Some(_), Some(_)) = (
        env(SESSION_WORKER_CONTROL_ADDRESS_ENV),
        env(SESSION_WORKER_CONTROL_TOKEN_ENV),
        env(SESSION_WORKER_SESSION_KEY_ENV),
    ) else {
        return Err(ChordError::error("Session worker requires a control address"));
    };
    let peer_id =
        env(SESSION_WORKER_PEER_ID_ENV).ok_or_else(|| ChordError::error("Session worker requires a peer ID"))?;
    let (mut reader, writer) = connect(Path::new(&address)).await.map_err(ChordError::other)?;
    let _ = FAILURE_WRITER.set(writer.clone());
    writer
        .write(&json!({ "type": "register_peer", "protocol": COORDINATOR_PROTOCOL_VERSION, "peerId": peer_id }))
        .await?;
    let registered = reader.next("Session worker control").await?;
    let rejected = || ChordError::error("Coordinator rejected the session worker registration");
    let registered = registered.filter(|value| value.get("type").and_then(Value::as_str) == Some("peer_registered"));
    let registered = registered.ok_or_else(rejected)?;
    let initial = registered.get("serverConnectionId").and_then(Value::as_str).map(str::to_string);
    Ok(Control { initial_server_connection_id: initial, reader, writer })
}

fn send(writer: &LineWriter, event: &SessionWorkerEvent) {
    writer.post(&json!({ "type": "send", "to": "server", "payload": event.to_value() }));
}

/// Parse the worker options, run the worker, and report startup failure to the server.
pub async fn run_session_worker_with_harness(args: &[String], create: CreateSessionWorkerHarness) -> ChordResult<()> {
    let result = run_checked(args, create).await;
    if let Err(error) = &result
        && let (Some(token), Some(session_key), Some(writer)) =
            (env(SESSION_WORKER_CONTROL_TOKEN_ENV), session_key(), FAILURE_WRITER.get())
    {
        let failed = SessionWorkerEvent::WorkerFailed { token, session_key, message: error.to_string() };
        let message = json!({ "type": "send", "to": "server", "payload": failed.to_value() });
        let _ = writer.write(&message).await;
    }
    result
}

async fn run_checked(args: &[String], create: CreateSessionWorkerHarness) -> ChordResult<()> {
    let [argument] = args else { return Err(ChordError::error("Session worker requires one options argument")) };
    let invalid = || ChordError::error("Session worker received invalid options");
    let options: SessionWorkerOptions = serde_json::from_str(argument).map_err(|_| invalid())?;
    let absolute = |path: &str| Path::new(path).is_absolute();
    let valid = !options.session_dir.is_empty()
        && !options.metadata.id.is_empty()
        && options.plugin_manifest_paths.iter().all(|path| !path.is_empty())
        && options.provider.as_deref() != Some("")
        && options.model.as_deref() != Some("")
        && absolute(&options.session_dir)
        && absolute(&options.metadata.cwd)
        && absolute(&options.metadata.path)
        && (options.provider.is_none() || options.model.is_some());
    if !valid {
        return Err(invalid());
    }
    run(options, create).await
}

async fn close_resources(
    runtime: Option<SessionWorkerRuntime>,
    services: Option<&SessionWorkerServices>,
    ownership: LockRelease,
) -> ChordResult<()> {
    let mut errors = Vec::new();
    if let Some(Err(error)) = match services {
        Some(services) => Some(services.dispose().await),
        None => None,
    } {
        errors.push(error);
    }
    if let Some(runtime) = runtime {
        if let Err(error) = runtime.harness.close(&TODO_CONTEXT).await {
            errors.push(error.into());
        }
        if let Some(cleanup) = runtime.cleanup
            && let Err(error) = cleanup(TODO_CONTEXT.clone()).await
        {
            errors.push(error);
        }
    }
    if let Err(error) = ownership.release().await {
        errors.push(error);
    }
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.remove(0)),
        _ => Err(ChordError::Aggregate { message: "Session worker cleanup failed".into(), errors }),
    }
}

async fn run(options: SessionWorkerOptions, create: CreateSessionWorkerHarness) -> ChordResult<()> {
    let metadata = options.metadata.clone();
    let Control { initial_server_connection_id, mut reader, writer } = connect_control().await?;
    let token = env(SESSION_WORKER_CONTROL_TOKEN_ENV).unwrap_or_default();
    let session_key = session_key().unwrap_or_default();
    let lock_options = LockOptions {
        realpath: true,
        stale: Duration::from_secs(2),
        update: Duration::from_secs(1),
        retry_interval: Duration::from_millis(25),
        max_retry_time: Duration::from_secs(8),
    };
    let ownership = lock_file(Path::new(&metadata.path), lock_options).await?;
    let runtime = match create(session_storage_path(&metadata), options.clone()).await {
        Ok(runtime) => runtime,
        Err(error) => {
            return Err(match close_resources(None, None, ownership).await {
                Ok(()) => error,
                Err(cleanup) => {
                    ChordError::with_cleanup(error, vec![cleanup], "Session worker startup and cleanup failed")
                }
            });
        }
    };
    let opened = async {
        let graph = runtime.harness.task_graph(&TODO_CONTEXT).await?;
        let (publish_writer, publish_token, publish_key) = (writer.clone(), token.clone(), session_key.clone());
        let publish = Arc::new(
            move |scope: &WorkerOperationScope,
                  subscription_id: &str,
                  update: pi_chord::services::ServiceProviderUpdate| {
                let event = SessionWorkerEvent::ServiceUpdate {
                    token: publish_token.clone(),
                    session_key: publish_key.clone(),
                    scope: scope.clone(),
                    subscription_id: subscription_id.to_string(),
                    update: update.to_json().to_serde(),
                };
                send(&publish_writer, &event);
            },
        );
        let services = create_session_worker_services(SessionWorkerServicesOptions {
            harness: &runtime.harness,
            conversation: &runtime.conversation,
            model_catalog: runtime.model_catalog.clone(),
            model_defaults: runtime.model_defaults.clone(),
            facet_loader: runtime.facet_loader.clone(),
            publish,
        });
        Ok::<_, ChordError>((graph, services.await?))
    };
    let (graph, services) = match opened.await {
        Ok(opened) => opened,
        Err(error) => {
            return Err(match close_resources(Some(runtime), None, ownership).await {
                Ok(()) => error,
                Err(cleanup) => {
                    ChordError::with_cleanup(error, vec![cleanup], "Session worker startup and cleanup failed")
                }
            });
        }
    };
    let services = Arc::new(services);
    let retire = Arc::new(Notify::new());
    let notify = retire.clone();
    let lifecycle = WorkerLifecycle::new(WorkerLifecycleOptions {
        initial_server_connection_id,
        initial_demand_grace: lifecycle_delay(SESSION_WORKER_INITIAL_DEMAND_GRACE_ENV, DEFAULT_INITIAL_DEMAND_GRACE)?,
        orphan_demand_grace: lifecycle_delay(SESSION_WORKER_ORPHAN_DEMAND_GRACE_ENV, DEFAULT_ORPHAN_DEMAND_GRACE)?,
        on_retire: Box::new(move || notify.notify_one()),
    });
    // Every live task, background work included, keeps the worker and its Session open.
    let activity = lifecycle.clone();
    let stop_activity = graph.subscribe(StateListener::sync(move |value: &Json, _context, _delivery| {
        let active = value.get("tasks").and_then(Json::as_object).is_some_and(|tasks| !tasks.is_empty());
        activity.set_harness_active(active);
        Ok(())
    }));
    // Recovered work from an interrupted turn continues now.
    let _ = runtime.harness.resume();
    let ready = SessionWorkerEvent::WorkerReady {
        token: token.clone(),
        session_key: session_key.clone(),
        session_id: metadata.id.clone(),
        pid: std::process::id(),
        metadata: metadata.clone(),
        plugin_manifest_paths: options.plugin_manifest_paths.clone(),
    };
    let worker = Worker {
        token,
        session_key,
        writer: writer.clone(),
        lifecycle: lifecycle.clone(),
        services: services.clone(),
        active: Arc::default(),
    };
    let announced = writer.write(&json!({ "type": "send", "to": "server", "payload": ready.to_value() })).await;
    let mut signals = signals()?;
    let failed = match announced {
        Err(error) => Some(error),
        Ok(()) => loop {
            tokio::select! {
                message = reader.next("Session worker control") => match message {
                    Ok(Some(message)) => if worker.handle(message, &ready).await.is_break() { break None },
                    _ => break None,
                },
                () = retire.notified() => break None,
                () = signals.recv() => break None,
            }
        },
    };
    lifecycle.close();
    stop_activity.unsubscribe();
    let _ = graph.dispose();
    services.remove_subscriptions(|_| true);
    for (_, (_, abort)) in lock(&worker.active).drain() {
        abort.abort(Some(ChordError::error("Session worker is closing")));
    }
    let closed = close_resources(Some(runtime), Some(&services), ownership).await;
    match (failed, closed) {
        (Some(error), Ok(())) => Err(error),
        (Some(error), Err(cleanup)) => {
            Err(ChordError::with_cleanup(error, vec![cleanup], "Session worker readiness and cleanup failed"))
        }
        (None, result) => {
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }
    }
}

type ActiveRequests = Arc<Mutex<HashMap<String, (WorkerOperationScope, AbortController)>>>;

struct Worker {
    token: String,
    session_key: String,
    writer: LineWriter,
    lifecycle: WorkerLifecycle,
    services: Arc<SessionWorkerServices>,
    active: ActiveRequests,
}

impl Worker {
    async fn handle(&self, message: Value, ready: &SessionWorkerEvent) -> std::ops::ControlFlow<()> {
        use std::ops::ControlFlow::{Break, Continue};
        let text = |key: &str| message.get(key).and_then(Value::as_str).map(str::to_string);
        match message.get("type").and_then(Value::as_str) {
            Some("server_connected") => match text("serverConnectionId") {
                Some(id) => self.lifecycle.server_connected(&id),
                None => return Break(()),
            },
            Some("server_disconnected") => {
                let Some(id) = text("serverConnectionId") else { return Break(()) };
                self.services.remove_subscriptions(|scope| scope.server_connection_id == id);
                for (scope, abort) in lock(&self.active).values() {
                    if scope.server_connection_id == id {
                        abort.abort(Some(ChordError::error("Server disconnected")));
                    }
                }
                self.lifecycle.server_disconnected(&id);
            }
            Some("message") if text("from").as_deref() == Some("server") => {
                let payload = message.get("payload").cloned().unwrap_or(Value::Null);
                let Some(command) = SessionWorkerCommand::parse(&payload) else { return Continue(()) };
                return self.command(command, ready).await;
            }
            Some("peer_registered") => {}
            _ => return Break(()),
        }
        Continue(())
    }

    async fn command(&self, command: SessionWorkerCommand, ready: &SessionWorkerEvent) -> std::ops::ControlFlow<()> {
        use std::ops::ControlFlow::{Break, Continue};
        match command {
            SessionWorkerCommand::Shutdown => return Break(()),
            SessionWorkerCommand::DiscoverWorkers => send(&self.writer, ready),
            SessionWorkerCommand::SessionDemand { server_connection_id, request_id, attachment_id, attached } => {
                let mut hold = self.lifecycle.hold_retirement();
                if !attached {
                    let matches = |scope: &WorkerOperationScope| {
                        scope.server_connection_id == server_connection_id && scope.attachment_id == attachment_id
                    };
                    self.services.remove_subscriptions(matches);
                }
                let (token, session_key) = (self.token.clone(), self.session_key.clone());
                let event = match self.lifecycle.set_demand(&server_connection_id, &attachment_id, attached) {
                    Err(error) => SessionWorkerEvent::DemandRejected {
                        token,
                        session_key,
                        request_id,
                        message: error.to_string(),
                    },
                    Ok(()) => {
                        SessionWorkerEvent::DemandApplied { token, session_key, request_id, attachment_id, attached }
                    }
                };
                send(&self.writer, &event);
                hold.release();
            }
            SessionWorkerCommand::OperationCancel { request_id, scope } => {
                if let Some((active, abort)) = lock(&self.active).get(&request_id)
                    && active == &scope
                {
                    abort.abort(Some(ChordError::Aborted));
                }
            }
            SessionWorkerCommand::Operation { request_id, scope, call } => self.operation(request_id, scope, call),
        }
        Continue(())
    }

    fn operation(&self, request_id: String, scope: WorkerOperationScope, call: Value) {
        let (context, abort) = with_cancel(&BACKGROUND_CONTEXT);
        let hold = self.lifecycle.begin_request(&scope.server_connection_id, &scope.attachment_id);
        if hold.is_ok() {
            lock(&self.active).insert(request_id.clone(), (scope.clone(), abort));
        }
        let (services, active, writer) = (self.services.clone(), self.active.clone(), self.writer.clone());
        let (token, session_key) = (self.token.clone(), self.session_key.clone());
        tokio::spawn(async move {
            let result = match &hold {
                Err(error) => Err(error.clone()),
                Ok(_) => match parse_service_call(&Json::from(&call)) {
                    Ok(call) => services.invoke(call, &scope, &context).await,
                    Err(error) => Err(error),
                },
            };
            lock(&active).remove(&request_id);
            let response = match result {
                Ok(result) => WorkerOperationResponse::OperationResult {
                    request_id,
                    scope,
                    result: result.map(|value| value.to_serde()),
                },
                Err(error) => WorkerOperationResponse::OperationError {
                    request_id,
                    scope,
                    code: error.remote_code().map(|code| code.as_str().to_string()),
                    message: error.to_string(),
                },
            };
            send(&writer, &SessionWorkerEvent::OperationResponse { token, session_key, response });
            drop(hold);
        });
    }
}
