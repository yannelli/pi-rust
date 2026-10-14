//! One connection to a pi-env daemon (upstream `connection.ts`): started lazily, and started again after it is lost.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_chord::AbortSignal;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::frame::{CANCEL, ERROR, EVENT, PING, REQUEST, RESULT, frame};
use crate::session::{start_session, write_frame};

pub use crate::frame::Json;

pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(5);
/// The daemon pings every five seconds; this much silence means the connection is gone.
pub(crate) const SILENCE_LIMIT: Duration = Duration::from_secs(30);
/// How long starting the daemon and its `hello` may take.
const START_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A failure reported by the daemon, with its Node-style code.
#[derive(Clone, Debug)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
    /// The error object as the daemon sent it.
    pub fields: Arc<Json>,
}

impl RemoteError {
    pub fn new(fields: Json) -> Self {
        let text = |key: &str| fields.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            code: text("code").unwrap_or_else(|| "unknown".into()),
            message: text("message").unwrap_or_else(|| "Remote operation failed".into()),
            path: text("path"),
            fields: Arc::new(fields),
        }
    }

    pub(crate) fn with(code: &str, message: &str, lost: bool) -> Self {
        let mut fields = Json::new();
        fields.insert("code".into(), code.into());
        fields.insert("message".into(), message.into());
        if lost {
            fields.insert("lost".into(), true.into());
        }
        Self::new(fields)
    }

    /// The connection failed or ended; a request may succeed once it is started again.
    pub(crate) fn lost(message: &str) -> Self {
        Self::with("unknown", message, true)
    }

    pub(crate) fn closed() -> Self {
        Self::with("unknown", "Connection closed", false)
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

/// Whether an error means the connection was lost, not that the daemon refused the request.
pub fn is_connection_lost(error: &RemoteError) -> bool {
    error.fields.get("lost") == Some(&Value::Bool(true))
}

/// What `hello` reports about the remote machine.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInfo {
    pub protocol: u64,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub home: String,
    pub tmpdir: String,
    /// Path separator: `/`, or `\` on Windows.
    #[serde(default)]
    pub separator: String,
    /// The daemon's working directory, where Node's `path.resolve` would fall back to `process.cwd()`.
    pub cwd: String,
    /// Windows: per-drive working directories (`C:` to `C:\work`) of the `=C:` variables.
    #[serde(default)]
    pub drive_cwds: BTreeMap<String, String>,
    pub pid: u32,
}

/// Computes the daemon command at each start; an `Err` message fails that start.
pub type CommandFn = Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<String>, String>> + Send + Sync>;
/// Receives the daemon's and the transport's diagnostic output.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Receives progress events of a request, such as `exec` output.
pub type EventFn = Arc<dyn Fn(&Json, &[u8]) + Send + Sync>;

/// The command that starts the daemon, before its `serve --token <hex>` arguments.
#[derive(Clone)]
pub enum DaemonCommand {
    Fixed(Vec<String>),
    /// Computed at each start (detecting and deploying first, for example); the next request retries a failure.
    Computed(CommandFn),
}

#[derive(Clone)]
pub struct ConnectionOptions {
    pub command: DaemonCommand,
    pub on_log: Option<LogFn>,
}

impl ConnectionOptions {
    pub fn new(command: Vec<String>) -> Self {
        Self { command: DaemonCommand::Fixed(command), on_log: None }
    }

    pub fn computed(command: CommandFn) -> Self {
        Self { command: DaemonCommand::Computed(command), on_log: None }
    }

    pub fn with_on_log(mut self, on_log: LogFn) -> Self {
        self.on_log = Some(on_log);
        self
    }
}

/// A reply of the daemon.
#[derive(Clone, Debug)]
pub struct Reply {
    pub json: Json,
    pub payload: Vec<u8>,
    /// The daemon session that answered; handles from it are only valid while it lives.
    pub session: u64,
}

#[derive(Default)]
pub struct RequestOptions {
    pub payload: Vec<u8>,
    /// Aborting sends `cancel`; the request still settles with the daemon's result or error.
    pub signal: Option<AbortSignal>,
    pub on_event: Option<EventFn>,
    /// The request's id and session, for `kill`.
    pub on_start: Option<Box<dyn FnOnce(u32, u64) + Send>>,
    /// Only run in this daemon session, for requests on handles it opened.
    pub session: Option<u64>,
}

pub(crate) struct Pending {
    pub reply: oneshot::Sender<Result<Reply, RemoteError>>,
    pub on_event: Option<EventFn>,
}

/// One running daemon.
pub(crate) struct Session {
    pub id: u64,
    pub pending: Mutex<HashMap<u32, Pending>>,
    pub live: AtomicBool,
    pub writer: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    pub kill: Mutex<Option<oneshot::Sender<()>>>,
    pub last_seen: Mutex<Instant>,
}

#[derive(Clone)]
struct Ready {
    info: Arc<RemoteInfo>,
    session: Arc<Session>,
}

type ReadyFuture = Shared<BoxFuture<'static, Result<Ready, RemoteError>>>;

#[derive(Default)]
struct State {
    session: Option<Arc<Session>>,
    ready: Option<(u64, ReadyFuture)>,
    sessions: u64,
    starts: u64,
    closed: bool,
}

pub(crate) struct Inner {
    pub options: ConnectionOptions,
    state: Mutex<State>,
    next_id: AtomicU32,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let session = lock(&self.state).session.take();
        if let Some(session) = session {
            session.teardown(RemoteError::closed());
        }
    }
}

impl Session {
    /// Mark the session dead, end its stdin, kill the daemon, and fail its pending requests.
    pub fn teardown(&self, error: RemoteError) -> bool {
        if !self.live.swap(false, Ordering::SeqCst) {
            return false;
        }
        lock(&self.writer).take();
        if let Some(kill) = lock(&self.kill).take() {
            let _ = kill.send(());
        }
        let pending: Vec<Pending> = lock(&self.pending).drain().map(|(_, pending)| pending).collect();
        for request in pending {
            let _ = request.reply.send(Err(error.clone()));
        }
        true
    }

    pub fn dispatch(&self, kind: u8, id: u32, json: Json, payload: Vec<u8>) {
        if kind == EVENT {
            let on_event = lock(&self.pending).get(&id).and_then(|pending| pending.on_event.clone());
            if let Some(on_event) = on_event {
                on_event(&json, &payload);
            }
            return;
        }
        if kind != RESULT && kind != ERROR {
            return;
        }
        let Some(pending) = lock(&self.pending).remove(&id) else { return };
        let reply =
            if kind == RESULT { Ok(Reply { json, payload, session: self.id }) } else { Err(RemoteError::new(json)) };
        let _ = pending.reply.send(reply);
    }
}

impl Inner {
    /// Tear down `session` and forget it if it is the current one.
    pub fn teardown(&self, session: &Arc<Session>, error: RemoteError) {
        if !session.teardown(error) {
            return;
        }
        let mut state = lock(&self.state);
        if state.session.as_ref().is_some_and(|current| Arc::ptr_eq(current, session)) {
            state.session = None;
            state.ready = None;
        }
    }

    pub fn ping(&self, session: &Arc<Session>) -> bool {
        if !session.live.load(Ordering::SeqCst) {
            return false;
        }
        write_frame(session, frame(PING, 0, &Json::new(), &[]));
        if lock(&session.last_seen).elapsed() > SILENCE_LIMIT {
            self.teardown(session, RemoteError::lost("pi-env connection timed out"));
            return false;
        }
        true
    }
}

/// Removes a request that is dropped before it settles, and cancels it in the daemon.
struct RequestGuard {
    session: Arc<Session>,
    id: u32,
    listener: Option<(AbortSignal, u64)>,
    done: bool,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some((signal, listener)) = self.listener.take() {
            signal.remove_listener(listener);
        }
        if !self.done && lock(&self.session.pending).remove(&self.id).is_some() {
            write_frame(&self.session, frame(CANCEL, self.id, &Json::new(), &[]));
        }
    }
}

/// One connection to a pi-env daemon: started lazily on the first request and started again after it is lost.
/// Requests in flight when it is lost fail with code `unknown`; for mutations their outcome is then unknown.
/// Clones share the connection; the daemon stops when the last clone is dropped or on `close`.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

impl Connection {
    pub fn new(options: ConnectionOptions) -> Self {
        Self { inner: Arc::new(Inner { options, state: Mutex::default(), next_id: AtomicU32::new(1) }) }
    }

    /// Connect if needed and return what the daemon reported about its machine.
    pub async fn info(&self) -> Result<Arc<RemoteInfo>, RemoteError> {
        Ok(self.connect().await?.info)
    }

    /// Connect if needed and return the live session's id.
    pub async fn session(&self) -> Result<u64, RemoteError> {
        Ok(self.connect().await?.session.id)
    }

    pub async fn request(&self, op: &str, json: Json, options: RequestOptions) -> Result<Reply, RemoteError> {
        if let Some(id) = options.session {
            let current = lock(&self.inner.state).session.clone();
            match current {
                Some(session) if session.id == id && session.live.load(Ordering::SeqCst) => {
                    return self.send(session, op, json, options).await;
                }
                _ => return Err(RemoteError::lost("pi-env connection lost")),
            }
        }
        let ready = self.connect().await?;
        self.send(ready.session, op, json, options).await
    }

    /// Send a request on the handles of session `session` now, so requests started one after another reach the daemon
    /// in that order; the returned future waits for the reply.
    pub fn request_now(
        &self,
        op: &str,
        mut json: Json,
        session: u64,
        options: RequestOptions,
    ) -> impl Future<Output = Result<Reply, RemoteError>> + Send + use<> {
        let current = lock(&self.inner.state).session.clone();
        let started = match current {
            Some(current) if current.id == session && current.live.load(Ordering::SeqCst) => {
                begin(&self.inner, current, op, &mut json, options)
            }
            _ => Err(RemoteError::lost("pi-env connection lost")),
        };
        async move { started?.await }
    }

    /// Stop the daemon; it kills everything it started.
    pub fn close(&self) {
        let session = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            state.session.clone()
        };
        if let Some(session) = session {
            self.inner.teardown(&session, RemoteError::closed());
        }
    }

    /// Kill one `exec` without aborting it.
    pub fn kill(&self, id: u32, session: u64) {
        let current = lock(&self.inner.state).session.clone();
        if let Some(current) = current.filter(|current| current.id == session) {
            let mut json = Json::new();
            json.insert("mode".into(), "kill".into());
            write_frame(&current, frame(CANCEL, id, &json, &[]));
        }
    }

    async fn connect(&self) -> Result<Ready, RemoteError> {
        let (generation, future) = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err(RemoteError::closed());
            }
            match &state.ready {
                Some((generation, future)) => (*generation, future.clone()),
                None => {
                    state.starts += 1;
                    let inner = Arc::downgrade(&self.inner);
                    let future = async move {
                        let Some(inner) = inner.upgrade() else { return Err(RemoteError::closed()) };
                        match tokio::spawn(start(inner)).await {
                            Ok(result) => result,
                            Err(error) => Err(RemoteError::lost(&error.to_string())),
                        }
                    }
                    .boxed()
                    .shared();
                    state.ready = Some((state.starts, future.clone()));
                    (state.starts, future)
                }
            }
        };
        let result = future.await;
        if result.is_err() {
            // A failed start is not remembered: the next request tries again.
            let mut state = lock(&self.inner.state);
            if state.ready.as_ref().is_some_and(|(current, _)| *current == generation) {
                state.ready = None;
            }
        }
        result
    }

    async fn send(
        &self,
        session: Arc<Session>,
        op: &str,
        mut json: Json,
        options: RequestOptions,
    ) -> Result<Reply, RemoteError> {
        send(&self.inner, session, op, &mut json, options).await
    }
}

async fn send(
    inner: &Inner,
    session: Arc<Session>,
    op: &str,
    json: &mut Json,
    options: RequestOptions,
) -> Result<Reply, RemoteError> {
    begin(inner, session, op, json, options)?.await
}

/// Register and write a request now; the returned future waits for its reply.
fn begin(
    inner: &Inner,
    session: Arc<Session>,
    op: &str,
    json: &mut Json,
    options: RequestOptions,
) -> Result<impl Future<Output = Result<Reply, RemoteError>> + Send + use<>, RemoteError> {
    if !session.live.load(Ordering::SeqCst) {
        return Err(RemoteError::lost("pi-env connection lost"));
    }
    let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
    let (reply, receiver) = oneshot::channel();
    lock(&session.pending).insert(id, Pending { reply, on_event: options.on_event });
    if !session.live.load(Ordering::SeqCst) {
        lock(&session.pending).remove(&id);
        return Err(RemoteError::lost("pi-env connection lost"));
    }
    let mut guard = RequestGuard { session: session.clone(), id, listener: None, done: false };
    if let Some(on_start) = options.on_start {
        on_start(id, session.id);
    }
    json.insert("op".into(), op.into());
    write_frame(&session, frame(REQUEST, id, json, &options.payload));
    if let Some(signal) = options.signal {
        let target = session.clone();
        let listener = signal.add_listener(move |_| write_frame(&target, frame(CANCEL, id, &Json::new(), &[])));
        guard.listener = Some((signal, listener));
    }
    Ok(async move {
        let mut guard = guard;
        let result = receiver.await.unwrap_or_else(|_| Err(RemoteError::lost("pi-env connection lost")));
        guard.done = true;
        result
    })
}

async fn start(inner: Arc<Inner>) -> Result<Ready, RemoteError> {
    let command = match &inner.options.command {
        DaemonCommand::Fixed(command) => command.clone(),
        DaemonCommand::Computed(compute) => {
            compute().await.map_err(|message| RemoteError::with("spawn_error", &message, true))?
        }
    };
    let id = {
        let mut state = lock(&inner.state);
        if state.closed {
            return Err(RemoteError::closed());
        }
        state.sessions += 1;
        state.sessions
    };
    let session = start_session(Arc::downgrade(&inner), &inner.options, &command, id)?;
    lock(&inner.state).session = Some(session.clone());
    let mut hello = Json::new();
    hello.insert("protocol".into(), 1.into());
    let reply =
        tokio::time::timeout(START_TIMEOUT, send(&inner, session.clone(), "hello", &mut hello, Default::default()))
            .await
            .unwrap_or_else(|_| Err(RemoteError::lost("pi-env did not answer within 60 s")));
    let result = reply.and_then(|reply| {
        let protocol = reply.json.get("protocol").cloned().unwrap_or(Value::Null);
        if protocol != 1 {
            return Err(RemoteError::lost(&format!("Unsupported protocol {protocol}")));
        }
        serde_json::from_value::<RemoteInfo>(Value::Object(reply.json))
            .map_err(|error| RemoteError::lost(&format!("Invalid hello from pi-env: {error}")))
    });
    match result {
        Ok(info) => Ok(Ready { info: Arc::new(info), session }),
        Err(error) => {
            inner.teardown(&session, error.clone());
            Err(error)
        }
    }
}
