//! `worker_threads.Worker`: a new pi-js runtime on its own OS thread. The parent's JavaScript
//! builds the worker's start-up code; this runs it and reports online, output, and exit.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rquickjs::{Ctx, Function, Object, Persistent, Value};
use serde::Deserialize;

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_to_js, error_value, throw};
use crate::runtime::state::{Message, state};
use crate::runtime::{JsHandle, JsRuntime, JsRuntimeOptions, OutputSink, OutputStream, UncaughtKind, report_error};

type Sender = std::sync::mpsc::Sender<Message>;
type Slot = Arc<Mutex<Option<JsHandle>>>;

static THREAD_IDS: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpawnOptions {
    name: String,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    init: String,
    #[serde(default)]
    stdout: bool,
    #[serde(default)]
    stderr: bool,
    memory_limit: Option<usize>,
    stack_size: Option<usize>,
}

struct WorkerEntry {
    referenced: bool,
    handle: Slot,
    terminated: Arc<AtomicBool>,
}

enum Event {
    Online,
    Output(OutputStream, Vec<u8>),
    Error(JsError),
    Exit(i32),
}

fn post(sender: &Sender, id: u64, event: Event) {
    let _ = sender.send(Message::Task(Box::new(move |ctx| deliver(ctx, id, event))));
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let state = state();
    let callback = state.callbacks.borrow().get(&id).cloned();
    let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else { return };
    let result = match event {
        Event::Online => function.call::<_, ()>(("online",)),
        Event::Output(stream, bytes) => {
            let name = if stream == OutputStream::Stdout { "stdout" } else { "stderr" };
            bytes_to_js(ctx, bytes).and_then(|bytes| function.call::<_, ()>((name, bytes)))
        }
        Event::Error(error) => error_value(ctx, &error).and_then(|error| function.call::<_, ()>(("error", error))),
        Event::Exit(code) => {
            if let Some(resource) = state.resources.borrow_mut().remove(&id)
                && resource.downcast_ref::<WorkerEntry>().is_some_and(|entry| entry.referenced)
            {
                state.remove_busy();
            }
            state.callbacks.borrow_mut().remove(&id);
            function.call::<_, ()>(("exit", code))
        }
    };
    if let Err(error) = result {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

/// The worker's output: forwarded to the parent's `worker.stdout`/`stderr` streams when asked,
/// otherwise written straight to the parent's sink.
struct WorkerOutput {
    parent: Sender,
    id: u64,
    sink: Arc<dyn OutputSink>,
    stdout: bool,
    stderr: bool,
}

impl OutputSink for WorkerOutput {
    fn write(&self, stream: OutputStream, bytes: &[u8]) {
        let forward = if stream == OutputStream::Stdout { self.stdout } else { self.stderr };
        if forward {
            post(&self.parent, self.id, Event::Output(stream, bytes.to_vec()));
        } else {
            self.sink.write(stream, bytes);
        }
    }

    /// A worker's children inherit the process's descriptors in Node, whatever the worker's streams.
    fn is_process_stdio(&self) -> bool {
        self.sink.is_process_stdio()
    }
}

fn run_worker(options: JsRuntimeOptions, init: String, parent: Sender, id: u64, slot: Slot, stop: Arc<AtomicBool>) {
    let code = Arc::new(AtomicI32::new(i32::MIN));
    let mut options = options;
    let exit_code = code.clone();
    options.on_exit = Some(Arc::new(move |value| {
        let _ = exit_code.compare_exchange(i32::MIN, value, Ordering::SeqCst, Ordering::SeqCst);
    }));
    let runtime = match JsRuntime::new(options) {
        Ok(runtime) => runtime,
        Err(error) => {
            post(&parent, id, Event::Error(error));
            return post(&parent, id, Event::Exit(1));
        }
    };
    *slot.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(runtime.handle());
    let result = if stop.load(Ordering::SeqCst) {
        Err(JsError::new("terminated"))
    } else {
        post(&parent, id, Event::Online);
        futures::executor::block_on(async {
            runtime.eval(init).await?;
            runtime.idle().await?;
            let before = "if (process.listenerCount('beforeExit') > 0) process.emit('beforeExit', process.exitCode ?? 0); void 0";
            runtime.eval(before).await?;
            runtime.idle().await?;
            runtime.eval("process.exit(); void 0").await.map(|_| ())
        })
    };
    slot.lock().unwrap_or_else(|poison| poison.into_inner()).take();
    drop(runtime);
    let stored = code.load(Ordering::SeqCst);
    let code = match () {
        () if stop.load(Ordering::SeqCst) => 1,
        () if stored != i32::MIN => stored,
        () => i32::from(result.is_err()),
    };
    post(&parent, id, Event::Exit(code));
}

fn spawn<'js>(ctx: Ctx<'js>, options: Value<'js>, callback: Function<'js>) -> rquickjs::Result<u64> {
    let spawn: SpawnOptions = super::net::parse_options(&ctx, options)?;
    let state = state();
    let id = state.next_id();
    let shared = state.shared.clone();
    let mut options = JsRuntimeOptions::new(state.cwd.borrow().clone());
    options.name = spawn.name;
    options.argv = spawn.argv;
    options.env = Some(spawn.env);
    options.tokio = Some(shared.tokio.clone());
    options.transpile = shared.transpiler.options().clone();
    options.virtual_modules = shared.virtual_modules.read().unwrap_or_else(|poison| poison.into_inner()).clone();
    options.host_functions = shared.host_functions.read().unwrap_or_else(|poison| poison.into_inner()).clone();
    options.memory_limit = spawn.memory_limit;
    if let Some(stack) = spawn.stack_size {
        options.max_stack_size = stack.clamp(256 * 1024, 12 * 1024 * 1024);
    }
    let parent = state.sender.clone();
    options.output = Arc::new(WorkerOutput {
        parent: parent.clone(),
        id,
        sink: shared.output.clone(),
        stdout: spawn.stdout,
        stderr: spawn.stderr,
    });
    let slot: Slot = Arc::default();
    let terminated = Arc::new(AtomicBool::new(false));
    let (thread_slot, stop) = (slot.clone(), terminated.clone());
    std::thread::Builder::new()
        .name(format!("pi-js-worker-{id}"))
        .spawn(move || run_worker(options, spawn.init, parent, id, thread_slot, stop))
        .map_err(|error| throw(&ctx, &JsError::from_io(&error, "spawn", None)))?;
    state.add_busy();
    state.resources.borrow_mut().insert(id, Box::new(WorkerEntry { referenced: true, handle: slot, terminated }));
    state.callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
    Ok(id)
}

fn terminate(id: u64) {
    let _ = state().with_resource(id, |entry: &mut WorkerEntry| {
        entry.terminated.store(true, Ordering::SeqCst);
        if let Some(handle) = entry.handle.lock().unwrap_or_else(|poison| poison.into_inner()).clone() {
            handle.interrupt();
            let _ = handle.send(Message::Shutdown);
        }
    });
}

fn set_ref(id: u64, referenced: bool) {
    let state = state();
    let changed =
        state.with_resource(id, |entry: &mut WorkerEntry| std::mem::replace(&mut entry.referenced, referenced));
    match changed {
        Ok(previous) if previous != referenced && referenced => state.add_busy(),
        Ok(previous) if previous != referenced => state.remove_busy(),
        _ => {}
    }
}

pub(super) fn natives(object: &Object<'_>) -> rquickjs::Result<()> {
    set_fn(object, "nextThreadId", || THREAD_IDS.fetch_add(1, Ordering::Relaxed))?;
    set_fn(object, "spawn", spawn)?;
    set_fn(object, "terminate", terminate)?;
    set_fn(object, "ref", set_ref)?;
    Ok(())
}
