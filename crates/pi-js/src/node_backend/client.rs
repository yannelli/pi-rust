//! The Rust end of the protocol: writes requests to the child and matches replies to waiters.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock, mpsc};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::error::JsError;
use crate::host::HostValue;
use crate::resolve::normalize;

pub(crate) type Answer = Result<HostValue, JsError>;

pub(crate) enum Waiter {
    Async(tokio::sync::oneshot::Sender<Answer>),
    Blocking(mpsc::Sender<Answer>),
}

impl Waiter {
    pub fn send(self, answer: Answer) {
        match self {
            Self::Async(sender) => drop(sender.send(answer)),
            Self::Blocking(sender) => drop(sender.send(answer)),
        }
    }
}

pub(crate) fn shut_down() -> JsError {
    JsError::new("the JavaScript runtime has shut down").with_code("ERR_PI_JS_SHUT_DOWN")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) struct NodeClient {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    pending: Mutex<HashMap<u64, Waiter>>,
    next_id: AtomicU64,
    pub bridge_thread: OnceLock<ThreadId>,
    /// Directory whose (nonexistent) files stand for virtual modules.
    pub virtual_dir: PathBuf,
    /// Module sources from `eval_module`, by absolute path.
    pub inline_sources: RwLock<HashMap<String, std::sync::Arc<str>>>,
    /// `interrupt` or `process.exit` ended the worker on purpose.
    pub ending: AtomicBool,
    pub exit_reason: Mutex<Option<String>>,
}

impl NodeClient {
    pub fn new(child: Child, stdin: ChildStdin, virtual_dir: PathBuf) -> Self {
        Self {
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            pending: Mutex::default(),
            next_id: AtomicU64::new(1),
            bridge_thread: OnceLock::new(),
            virtual_dir,
            inline_sources: RwLock::default(),
            ending: AtomicBool::new(false),
            exit_reason: Mutex::default(),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn write(&self, frame: &Value) -> Result<(), JsError> {
        let mut line = frame.to_string();
        line.push('\n');
        let mut stdin = lock(&self.stdin);
        let stdin = stdin.as_mut().ok_or_else(shut_down)?;
        stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).map_err(|_| shut_down())
    }

    /// Send `{t: "req", id, ...frame}`; the bridge hands the reply to `waiter`.
    pub fn send_request(
        &self,
        alive: &AtomicBool,
        mut frame: Map<String, Value>,
        waiter: Waiter,
    ) -> Result<(), JsError> {
        if !alive.load(Ordering::SeqCst) {
            return Err(self.exited());
        }
        let id = self.next_id();
        frame.insert("t".into(), json!("req"));
        frame.insert("id".into(), json!(id));
        lock(&self.pending).insert(id, waiter);
        let written = self.write(&Value::Object(frame));
        if written.is_err() || !alive.load(Ordering::SeqCst) {
            if let Some(waiter) = lock(&self.pending).remove(&id) {
                waiter.send(Err(self.exited()));
            }
        }
        Ok(())
    }

    pub fn complete(&self, id: u64, answer: Answer) {
        if let Some(waiter) = lock(&self.pending).remove(&id) {
            waiter.send(answer);
        }
    }

    /// Fail every waiting call; the child is gone.
    pub fn fail_all(&self) {
        let waiters: Vec<Waiter> = lock(&self.pending).drain().map(|(_, waiter)| waiter).collect();
        for waiter in waiters {
            waiter.send(Err(self.exited()));
        }
    }

    pub fn exited(&self) -> JsError {
        match lock(&self.exit_reason).clone() {
            Some(reason) => JsError::new(reason).with_code("ERR_PI_NODE_EXITED"),
            None => shut_down(),
        }
    }

    pub fn set_exit_reason(&self, reason: String) {
        lock(&self.exit_reason).get_or_insert(reason);
    }

    /// Make `name` (relative to `cwd`) load from `source`; returns its absolute key.
    pub fn register_inline(&self, cwd: &Path, name: &str, source: String) -> String {
        let path = Path::new(name);
        let key = if path.is_absolute() { normalize(path) } else { normalize(&cwd.join(path)) };
        let key = key.display().to_string();
        self.inline_sources.write().unwrap_or_else(|poison| poison.into_inner()).insert(key.clone(), source.into());
        key
    }

    pub fn release(&self, id: u64) {
        let _ = self.write(&json!({ "t": "release", "ids": [id] }));
    }

    pub fn interrupt(&self) {
        self.ending.store(true, Ordering::SeqCst);
        self.set_exit_reason("the Node worker was interrupted".into());
        let _ = self.write(&json!({ "t": "interrupt" }));
    }

    /// Close stdin so the child exits, then wait briefly and kill it if it is still running.
    pub fn shutdown(&self) {
        self.ending.store(true, Ordering::SeqCst);
        lock(&self.stdin).take();
        let Some(mut child) = lock(&self.child).take() else { return };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for NodeClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}
