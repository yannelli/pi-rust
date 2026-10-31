//! `RemoteWatcher` (upstream `watch.ts`): Durable's local file watcher running in the daemon, next to the files.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::BoxFuture;
use pi_chord::{AbortController, Context};
use pi_durable::env::{FileChange, FileError, FileErrorCode, FileWatcher, WatchListener, WatchMode, WatchTarget};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use crate::connection::{Connection, Json, RemoteError, RequestOptions, is_connection_lost, lock};
use crate::errors::to_file_error;

const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// How the daemon watches, like the local env's watch options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteWatchOptions {
    /// Force a mode; by default Windows and file systems that do not report remote changes poll.
    pub mode: Option<WatchMode>,
    /// Interval between snapshots in `polling` mode; default 2000 ms.
    pub poll_interval_ms: Option<u64>,
    /// Most directories one watcher covers; default 10,000.
    pub max_directories: Option<u64>,
}

struct WatchState {
    connection: Connection,
    request: Json,
    on_change: WatchListener,
    mode: Mutex<WatchMode>,
    closed: AtomicBool,
    controller: Mutex<Option<AbortController>>,
    task: Mutex<Option<JoinHandle<()>>>,
    wake: Notify,
}

impl WatchState {
    fn deliver(&self, change: FileChange) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        // A panicking callback must not stop watching.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| (self.on_change)(change)));
    }
}

/// A watcher running in the daemon. When the connection is lost, it is opened again once the connection can be
/// started again, and reports `Overflow`: changes in between were not seen.
pub struct RemoteWatcher {
    state: Arc<WatchState>,
}

type ReadySender = Arc<Mutex<Option<oneshot::Sender<Result<(), RemoteError>>>>>;

impl RemoteWatcher {
    /// Watch `targets` (resolved paths); fails like the local watcher if coverage cannot be established.
    pub async fn open(
        connection: Connection,
        targets: Vec<WatchTarget>,
        on_change: WatchListener,
        options: RemoteWatchOptions,
    ) -> Result<Self, RemoteError> {
        let targets: Vec<Value> = targets
            .iter()
            .map(|target| {
                let exclude = target.exclude.clone().unwrap_or_default();
                json!({
                    "path": target.path,
                    "recursive": target.recursive,
                    "exclude": { "hidden": exclude.hidden, "names": exclude.names },
                })
            })
            .collect();
        let mut request = Json::new();
        request.insert("targets".into(), targets.into());
        if let Some(mode) = options.mode {
            request.insert("mode".into(), json!(mode));
        }
        if let Some(interval) = options.poll_interval_ms {
            request.insert("pollIntervalMs".into(), interval.into());
        }
        if let Some(max) = options.max_directories {
            request.insert("maxDirectories".into(), max.into());
        }
        let state = Arc::new(WatchState {
            connection,
            request,
            on_change,
            mode: Mutex::new(WatchMode::Native),
            closed: AtomicBool::new(false),
            controller: Mutex::new(None),
            task: Mutex::new(None),
            wake: Notify::new(),
        });
        start(state.clone()).await?;
        Ok(Self { state })
    }
}

impl Drop for RemoteWatcher {
    fn drop(&mut self) {
        // Dropped without `close`: stop the daemon's watcher anyway.
        self.state.closed.store(true, Ordering::SeqCst);
        self.state.wake.notify_one();
        if let Some(controller) = lock(&self.state.controller).as_ref() {
            controller.abort(None);
        }
    }
}

#[async_trait]
impl FileWatcher for RemoteWatcher {
    fn mode(&self) -> WatchMode {
        *lock(&self.state.mode)
    }

    async fn stop(&self, _context: &Context) {
        if self.state.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.state.wake.notify_one();
        if let Some(controller) = lock(&self.state.controller).as_ref() {
            controller.abort(None);
        }
        let task = lock(&self.state.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

fn handle_event(state: &WatchState, event: &Json, ready: &AtomicBool, sender: &ReadySender) {
    let mode = |value: Option<&Value>| match value.and_then(Value::as_str) {
        Some("polling") => Some(WatchMode::Polling),
        Some("native") => Some(WatchMode::Native),
        _ => None,
    };
    match event.get("kind").and_then(Value::as_str) {
        Some("ready") => {
            ready.store(true, Ordering::SeqCst);
            *lock(&state.mode) = mode(event.get("mode")).unwrap_or(WatchMode::Native);
            if let Some(sender) = lock(sender).take() {
                let _ = sender.send(Ok(()));
            }
        }
        Some("change") => {
            if mode(event.get("mode")) == Some(WatchMode::Polling) {
                *lock(&state.mode) = WatchMode::Polling;
            }
            if event.get("overflow") == Some(&Value::Bool(true)) {
                state.deliver(FileChange::Overflow);
            } else {
                let paths = event.get("paths").and_then(Value::as_array).into_iter().flatten();
                state.deliver(FileChange::Paths(paths.filter_map(Value::as_str).map(str::to_string).collect()));
            }
        }
        Some("error") => {
            let code = match event.get("code").and_then(Value::as_str) {
                Some("permission_denied") => FileErrorCode::PermissionDenied,
                _ => FileErrorCode::Invalid,
            };
            let message = match event.get("message") {
                Some(Value::String(message)) => message.clone(),
                other => other.map_or("undefined".into(), Value::to_string),
            };
            state.deliver(FileChange::Failed(FileError::new(code, message, None)));
            state.closed.store(true, Ordering::SeqCst);
        }
        _ => {}
    }
}

/// Open the daemon's watcher; resolves once it reports coverage.
fn start(state: Arc<WatchState>) -> BoxFuture<'static, Result<(), RemoteError>> {
    async move {
        let (sender, receiver) = oneshot::channel();
        let sender: ReadySender = Arc::new(Mutex::new(Some(sender)));
        let ready = Arc::new(AtomicBool::new(false));
        let controller = AbortController::new();
        *lock(&state.controller) = Some(controller.clone());
        let (event_state, event_ready, event_sender) = (state.clone(), ready.clone(), sender.clone());
        let on_event =
            Arc::new(move |event: &Json, _: &[u8]| handle_event(&event_state, event, &event_ready, &event_sender));
        let options =
            RequestOptions { signal: Some(controller.signal()), on_event: Some(on_event), ..Default::default() };
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let result = task_state.connection.request("watch", task_state.request.clone(), options).await;
            let error = match result {
                Ok(_) => RemoteError::closed(),
                Err(error) => error,
            };
            if !ready.load(Ordering::SeqCst) {
                if let Some(sender) = lock(&sender).take() {
                    let _ = sender.send(Err(error));
                }
                return;
            }
            if task_state.closed.load(Ordering::SeqCst) {
                return;
            }
            if is_connection_lost(&error) {
                tokio::spawn(reconnect(task_state));
                return;
            }
            task_state.deliver(FileChange::Failed(to_file_error(&error, None)));
            task_state.closed.store(true, Ordering::SeqCst);
        });
        *lock(&state.task) = Some(task);
        receiver.await.unwrap_or_else(|_| Err(RemoteError::lost("pi-env connection lost")))
    }
    .boxed()
}

async fn reconnect(state: Arc<WatchState>) {
    let mut delay = RECONNECT_FIRST;
    while !state.closed.load(Ordering::SeqCst) {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = state.wake.notified() => {}
        }
        if state.closed.load(Ordering::SeqCst) {
            return;
        }
        match start(state.clone()).await {
            Ok(()) => {
                // Changes while disconnected were not seen.
                state.deliver(FileChange::Overflow);
                return;
            }
            Err(error) if is_connection_lost(&error) => delay = (delay * 2).min(RECONNECT_MAX),
            Err(error) => {
                state.deliver(FileChange::Failed(to_file_error(&error, None)));
                state.closed.store(true, Ordering::SeqCst);
                return;
            }
        }
    }
}
