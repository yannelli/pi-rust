//! `RemoteTerminal` and `open_terminal` (upstream `terminal.ts`): a program in a pseudo-terminal of a pi-env daemon.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_chord::AbortController;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::connection::{Connection, Json, RemoteError, RequestOptions, lock};

/// Input larger than this goes to the daemon in several writes; frames are at most 16 MiB.
const WRITE_CHUNK: usize = 1024 * 1024;

/// Receives the terminal's raw output, in order.
pub type DataFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct TerminalOptions {
    /// Working directory of the program; it must exist.
    pub cwd: String,
    /// Program and arguments. Default: the user's shell (`SHELL` in `env` or the daemon's environment) as a login
    /// shell.
    pub argv: Option<Vec<String>>,
    /// Added to the daemon's environment, or the whole environment with `inherit_env: Some(false)`. Set `TERM` here.
    pub env: Option<BTreeMap<String, String>>,
    pub inherit_env: Option<bool>,
    pub cols: u32,
    pub rows: u32,
    pub on_data: DataFn,
}

impl TerminalOptions {
    pub fn new(cwd: impl Into<String>, cols: u32, rows: u32, on_data: DataFn) -> Self {
        Self { cwd: cwd.into(), argv: None, env: None, inherit_env: None, cols, rows, on_data }
    }
}

/// A program running in a pseudo-terminal of a pi-env daemon. It ends when the program exits, `close()` kills it, or
/// the connection is lost (the daemon then kills it). Dropping the value does not end the program.
pub struct RemoteTerminal {
    pub pid: u32,
    connection: Connection,
    id: u32,
    session: u64,
    abort: AbortController,
    exited: Shared<BoxFuture<'static, Result<i64, RemoteError>>>,
}

impl RemoteTerminal {
    /// The exit code; fails with the daemon's error, with code `aborted` after `close()`.
    pub async fn exited(&self) -> Result<i64, RemoteError> {
        self.exited.clone().await
    }

    /// Send input; once the terminal has ended, input is dropped. Needs a tokio runtime, like the connection.
    pub fn write(&self, data: impl AsRef<[u8]>) {
        for chunk in data.as_ref().chunks(WRITE_CHUNK) {
            self.request("ptyWrite", Json::new(), chunk.to_vec());
        }
    }

    /// Change the size; the program receives `SIGWINCH`.
    pub fn resize(&self, cols: u32, rows: u32) {
        let mut json = Json::new();
        json.insert("cols".into(), cols.into());
        json.insert("rows".into(), rows.into());
        self.request("ptyResize", json, Vec::new());
    }

    /// Kill the program and everything in its session.
    pub fn close(&self) {
        self.abort.abort(None);
    }

    /// Sent at once, so calls made one after another reach the daemon in that order.
    fn request(&self, op: &str, mut json: Json, payload: Vec<u8>) {
        json.insert("target".into(), self.id.into());
        let options = RequestOptions { payload, ..Default::default() };
        let sent = self.connection.request_now(op, json, self.session, options);
        // A terminal that ended or a lost connection: `exited` reports it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = sent.await;
            });
        }
    }
}

/// Aborts the request when `open_terminal` is dropped before the program started.
struct OpenGuard(Option<AbortController>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if let Some(abort) = self.0.take() {
            abort.abort(None);
        }
    }
}

/// Start a program in a new pseudo-terminal of the daemon behind `connection`.
pub async fn open_terminal(connection: &Connection, options: TerminalOptions) -> Result<RemoteTerminal, RemoteError> {
    let abort = AbortController::new();
    let mut guard = OpenGuard(Some(abort.clone()));
    let mut request = Json::new();
    request.insert("cwd".into(), options.cwd.into());
    if let Some(argv) = options.argv {
        request.insert("argv".into(), json!(argv));
    }
    if let Some(env) = options.env {
        request.insert("env".into(), json!(env));
    }
    if let Some(inherit_env) = options.inherit_env {
        request.insert("inheritEnv".into(), inherit_env.into());
    }
    request.insert("cols".into(), options.cols.into());
    request.insert("rows".into(), options.rows.into());

    let (started, started_receiver) = oneshot::channel::<u32>();
    let started = Mutex::new(Some(started));
    let identity = Arc::new(Mutex::new(None::<(u32, u64)>));
    let on_data = options.on_data;
    let request_options = RequestOptions {
        signal: Some(abort.signal()),
        on_start: Some({
            let identity = identity.clone();
            Box::new(move |id, session| *lock(&identity) = Some((id, session)))
        }),
        on_event: Some(Arc::new(move |event: &Json, payload: &[u8]| match event.get("kind").and_then(Value::as_str) {
            // A panicking callback must not end the connection.
            Some("output") => {
                let _ = std::panic::catch_unwind(AssertUnwindSafe(|| on_data(payload)));
            }
            Some("started") => {
                if let Some(started) = lock(&started).take() {
                    let _ = started.send(event.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32);
                }
            }
            _ => {}
        })),
        ..Default::default()
    };
    let running = tokio::spawn({
        let connection = connection.clone();
        async move {
            let reply = connection.request("pty", request, request_options).await?;
            reply
                .json
                .get("exitCode")
                .and_then(Value::as_i64)
                .ok_or_else(|| RemoteError::lost("Invalid pty result from pi-env"))
        }
    });
    let exited: Shared<BoxFuture<'static, Result<i64, RemoteError>>> =
        async move { running.await.unwrap_or_else(|error| Err(RemoteError::lost(&error.to_string()))) }
            .boxed()
            .shared();
    // Failing before it started fails the open; afterwards, `exited` carries it.
    let pid = tokio::select! {
        biased;
        started = started_receiver => started.ok(),
        _ = exited.clone() => None,
    };
    let (Some(pid), Some((id, session))) = (pid, *lock(&identity)) else {
        return Err(exited.await.err().unwrap_or_else(|| RemoteError::lost("pi-env terminal ended before it started")));
    };
    guard.0 = None;
    Ok(RemoteTerminal { pid, connection: connection.clone(), id, session, abort, exited })
}
