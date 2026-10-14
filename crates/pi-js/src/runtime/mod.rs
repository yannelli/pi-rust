//! One QuickJS runtime on its own thread, with an event loop and a `Send` handle.

pub(crate) mod convert;
mod event_loop;
mod handle;
pub(crate) mod scope;
pub(crate) mod state;
mod timers;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread::JoinHandle;

use rquickjs::{Ctx, Function, Value};

pub use handle::{JsHandle, JsRef};
pub use scope::JsScope;

use self::convert::{caught, error_from_value};
use self::event_loop::LoopSetup;
use self::state::{LoopState, Message, Shared, state};
use crate::error::JsError;
use crate::host::HostFunctions;
use crate::resolve::ModuleResolver;
use crate::transpile::{TranspileCache, TranspileOptions};
use crate::virtual_modules::VirtualModules;

/// Native stack of the JavaScript thread, and the part QuickJS may use before a `RangeError`.
const THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_JS_STACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Where `console`, `process.stdout`, and `process.stderr` write.
pub trait OutputSink: Send + Sync {
    fn write(&self, stream: OutputStream, bytes: &[u8]);
    fn is_tty(&self, _stream: OutputStream) -> bool {
        false
    }
    /// `(columns, rows)` reported by `process.stdout`.
    fn terminal_size(&self) -> Option<(u16, u16)> {
        None
    }
}

/// Writes to the process's own stdout and stderr.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdOutput;

impl OutputSink for StdOutput {
    fn write(&self, stream: OutputStream, bytes: &[u8]) {
        // Flushed at once: process.exit ends the process without running destructors.
        let _ = match stream {
            OutputStream::Stdout => std::io::stdout().lock().write_all(bytes).and_then(|()| std::io::stdout().flush()),
            OutputStream::Stderr => std::io::stderr().lock().write_all(bytes),
        };
    }

    fn is_tty(&self, stream: OutputStream) -> bool {
        match stream {
            OutputStream::Stdout => std::io::stdout().is_terminal(),
            OutputStream::Stderr => std::io::stderr().is_terminal(),
        }
    }

    fn terminal_size(&self) -> Option<(u16, u16)> {
        let read = |name: &str| std::env::var(name).ok()?.parse::<u16>().ok();
        Some((read("COLUMNS")?, read("LINES").unwrap_or(24)))
    }
}

/// An exception nobody caught, or a promise rejection nobody handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UncaughtKind {
    Exception,
    Rejection,
}

pub type UncaughtCallback = Arc<dyn Fn(UncaughtKind, &JsError) + Send + Sync>;
pub type ExitCallback = Arc<dyn Fn(i32) + Send + Sync>;

/// Settings for [`JsRuntime::new`].
#[derive(Clone)]
pub struct JsRuntimeOptions {
    /// Thread name, also used for the owned tokio runtime's threads.
    pub name: String,
    pub cwd: PathBuf,
    /// `process.argv`; defaults to the current executable alone.
    pub argv: Vec<String>,
    /// `process.env`; `None` copies the current process environment.
    pub env: Option<BTreeMap<String, String>>,
    pub memory_limit: Option<usize>,
    pub max_stack_size: usize,
    /// Tokio runtime for host work; `None` starts a private two-thread runtime.
    pub tokio: Option<tokio::runtime::Handle>,
    pub transpile: TranspileOptions,
    /// Directory for the on-disk transpile cache; `None` keeps it in memory.
    pub cache_dir: Option<PathBuf>,
    pub virtual_modules: VirtualModules,
    pub host_functions: HostFunctions,
    pub output: Arc<dyn OutputSink>,
    /// Receives uncaught errors; by default they print to the stderr sink.
    pub on_uncaught: Option<UncaughtCallback>,
    /// Called by `process.exit(code)`; by default the whole process exits, as in Node.
    pub on_exit: Option<ExitCallback>,
}

impl JsRuntimeOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let exe = std::env::current_exe().map(|path| path.display().to_string()).unwrap_or_else(|_| "pi".into());
        Self {
            name: "pi-js".into(),
            cwd: cwd.into(),
            argv: vec![exe],
            env: None,
            memory_limit: None,
            max_stack_size: DEFAULT_MAX_JS_STACK_BYTES,
            tokio: None,
            transpile: TranspileOptions::default(),
            cache_dir: None,
            virtual_modules: VirtualModules::default(),
            host_functions: HostFunctions::default(),
            output: Arc::new(StdOutput),
            on_uncaught: None,
            on_exit: None,
        }
    }
}

/// Configuration the JavaScript thread reads.
pub(crate) struct RuntimeConfig {
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub on_uncaught: Option<UncaughtCallback>,
    pub on_exit: Option<ExitCallback>,
}

/// Owns the JavaScript thread. Dropping it stops the event loop and joins the thread.
pub struct JsRuntime {
    handle: JsHandle,
    thread: Option<JoinHandle<()>>,
    owned_tokio: Option<tokio::runtime::Runtime>,
}

impl JsRuntime {
    /// Start the thread, run the bootstrap, and return once the runtime accepts work.
    pub fn new(options: JsRuntimeOptions) -> Result<Self, JsError> {
        let (owned_tokio, tokio) = match options.tokio.clone() {
            Some(handle) => (None, handle),
            None => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name(format!("{}-io", options.name))
                    .enable_all()
                    .build()?;
                let handle = runtime.handle().clone();
                (Some(runtime), handle)
            }
        };
        let env = options.env.clone().unwrap_or_else(|| std::env::vars().collect());
        let shared = Arc::new(Shared {
            tokio,
            virtual_modules: RwLock::new(options.virtual_modules.clone()),
            host_functions: RwLock::new(options.host_functions.clone()),
            transpiler: TranspileCache::new(options.transpile.clone(), options.cache_dir.clone()),
            resolver: ModuleResolver::new(),
            files: Default::default(),
            http: Default::default(),
            source_files: Default::default(),
            interrupt: Arc::new(AtomicBool::new(false)),
            alive: AtomicBool::new(false),
            config: RuntimeConfig {
                cwd: crate::resolve::canonical(&options.cwd),
                argv: options.argv.clone(),
                env,
                on_uncaught: options.on_uncaught.clone(),
                on_exit: options.on_exit.clone(),
            },
            output: options.output.clone(),
        });
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready) = mpsc::channel();
        let setup = LoopSetup { memory_limit: options.memory_limit, max_stack_size: options.max_stack_size };
        let thread_shared = shared.clone();
        let thread_sender = sender.clone();
        let thread = std::thread::Builder::new()
            .name(options.name.clone())
            .stack_size(THREAD_STACK_BYTES)
            .spawn(move || thread_main(thread_shared, thread_sender, receiver, setup, ready_sender))?;
        let started = ready.recv().unwrap_or_else(|_| Err(JsError::new("the JavaScript thread exited during startup")));
        let runtime = Self { handle: JsHandle::new(sender, shared), thread: Some(thread), owned_tokio };
        started.map(|()| runtime)
    }

    pub fn handle(&self) -> JsHandle {
        self.handle.clone()
    }
}

impl std::ops::Deref for JsRuntime {
    type Target = JsHandle;

    fn deref(&self) -> &JsHandle {
        &self.handle
    }
}

impl Drop for JsRuntime {
    fn drop(&mut self) {
        let _ = self.handle.send(Message::Shutdown);
        if let Some(thread) = self.thread.take()
            && !self.handle.on_js_thread()
        {
            let _ = thread.join();
        }
        if let Some(runtime) = self.owned_tokio.take() {
            runtime.shutdown_background();
        }
    }
}

fn thread_main(
    shared: Arc<Shared>,
    sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    setup: LoopSetup,
    ready: mpsc::Sender<Result<(), JsError>>,
) {
    // Host functions on this thread may spawn tasks on the runtime's tokio runtime.
    let tokio = shared.tokio.clone();
    let _entered = tokio.enter();
    let state = LoopState::install(shared.clone(), sender);
    let (runtime, context) = match event_loop::create(&setup, &state) {
        Ok(pair) => pair,
        Err(error) => {
            state.release_all();
            let _ = ready.send(Err(error));
            return;
        }
    };
    let booted = context.with(|ctx| crate::node::install(&ctx).map_err(|error| caught(&ctx, error)));
    if let Err(error) = booted {
        state.release_all();
        let _ = ready.send(Err(error));
        return;
    }
    shared.alive.store(true, Ordering::SeqCst);
    let _ = ready.send(Ok(()));
    event_loop::run(&runtime, &context, &receiver, &state);
    shared.alive.store(false, Ordering::SeqCst);
    state.release_all();
    drop(receiver);
    drop(context);
    drop(runtime);
}

/// Hand an uncaught value to `process` listeners, then to the configured callback.
pub(crate) fn report_value<'js>(ctx: &Ctx<'js>, kind: UncaughtKind, value: Value<'js>) {
    let name = match kind {
        UncaughtKind::Exception => "uncaughtException",
        UncaughtKind::Rejection => "unhandledRejection",
    };
    let handled = state()
        .internal(ctx)
        .and_then(|internal| internal.get::<_, Function>("handleUncaught"))
        .and_then(|handler| handler.call::<_, bool>((name, value.clone())));
    match handled {
        Ok(true) => {}
        Ok(false) => deliver(kind, &error_from_value(ctx, value)),
        Err(error) => {
            let failure = caught(ctx, error);
            deliver(UncaughtKind::Exception, &failure);
        }
    }
}

pub(crate) fn report_error(ctx: &Ctx<'_>, kind: UncaughtKind, error: rquickjs::Error) {
    match error {
        rquickjs::Error::Exception => {
            let value = ctx.catch();
            report_value(ctx, kind, value);
        }
        other => deliver(kind, &JsError::new(other.to_string())),
    }
}

fn deliver(kind: UncaughtKind, error: &JsError) {
    let state = state();
    match &state.shared.config.on_uncaught {
        Some(callback) => callback(kind, error),
        None => {
            let prefix = if kind == UncaughtKind::Rejection { "Unhandled rejection: " } else { "Uncaught " };
            let text = format!("{prefix}{}\n", error.display_with_stack());
            state.shared.output.write(OutputStream::Stderr, text.as_bytes());
        }
    }
}
