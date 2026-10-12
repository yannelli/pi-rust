//! The QuickJS side of one execution: a dedicated thread with its own runtime, which relays tool
//! calls and output to the host and reports the result. The host stops it with the interrupt flag.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use rquickjs::context::EvalOptions;
use rquickjs::function::Opt;
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Function, Object, Persistent, Runtime, Value};
use tokio::sync::mpsc::UnboundedSender;

use super::types::CodemodeOutputItem;

const PRELUDE_SOURCE: &str = include_str!("prelude.js");
/// Native stack of the worker thread, and the part QuickJS may use before it throws a `RangeError`.
const THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const MAX_JS_STACK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallTarget {
    Tool,
    Global,
}

/// Messages to the host. Arguments, values, and errors cross as JSON text.
#[derive(Debug)]
pub(crate) enum WorkerMessage {
    Call {
        id: u64,
        target: CallTarget,
        name: String,
        args: Option<String>,
    },
    Output(CodemodeOutputItem),
    /// `writes` is a JSON array of `[key, json]` for `store()` and `[key]` for deletions.
    Done {
        ok: bool,
        value: Option<String>,
        writes: Option<String>,
        error: Option<String>,
    },
    /// The VM failed outside the script's control.
    Crash(String),
}

/// A settled host call: `payload` is the JSON result when `ok`, otherwise the error message.
#[derive(Debug)]
pub(crate) struct HostReply {
    pub id: u64,
    pub ok: bool,
    pub payload: Option<String>,
}

pub(crate) struct WorkerSetup {
    pub code: String,
    pub tools_json: String,
    pub globals_json: String,
    pub store_json: String,
    pub memory_limit: Option<usize>,
    pub interrupt: Arc<AtomicBool>,
    pub to_host: UnboundedSender<WorkerMessage>,
    pub replies: mpsc::Receiver<HostReply>,
}

/// Start the worker thread. The returned receiver resolves when the thread has exited.
pub(crate) fn spawn(setup: WorkerSetup) -> std::io::Result<tokio::sync::oneshot::Receiver<()>> {
    let (exited, on_exit) = tokio::sync::oneshot::channel();
    std::thread::Builder::new().name("codemode".into()).stack_size(THREAD_STACK_BYTES).spawn(move || {
        let to_host = setup.to_host.clone();
        let interrupt = setup.interrupt.clone();
        if let Err(message) = run(setup)
            && !interrupt.load(Ordering::SeqCst)
        {
            let _ = to_host.send(WorkerMessage::Crash(message));
        }
        drop(exited);
    })?;
    Ok(on_exit)
}

fn eval_options(filename: &str) -> EvalOptions {
    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(filename.into());
    options
}

fn js_string(value: &Value<'_>) -> Option<String> {
    value.as_string().and_then(|text| text.to_string().ok())
}

fn optional_string(value: Opt<Value<'_>>) -> Option<String> {
    value.0.as_ref().and_then(js_string)
}

fn bridge_message(kind: &str, a: Opt<Value<'_>>, b: Opt<Value<'_>>, c: Opt<Value<'_>>) -> Option<WorkerMessage> {
    match kind {
        "call" | "global" => Some(WorkerMessage::Call {
            id: a.0.as_ref()?.as_number()? as u64,
            target: if kind == "call" { CallTarget::Tool } else { CallTarget::Global },
            name: optional_string(b)?,
            args: optional_string(c),
        }),
        "output" => {
            let data = optional_string(b)?;
            Some(WorkerMessage::Output(match optional_string(a)?.as_str() {
                "image" => CodemodeOutputItem::Image { data, mime_type: optional_string(c)? },
                "console" => CodemodeOutputItem::Text { text: data, console: true },
                _ => CodemodeOutputItem::Text { text: data, console: false },
            }))
        }
        "done" => {
            if a.0.as_ref()?.as_bool()? {
                Some(WorkerMessage::Done {
                    ok: true,
                    value: optional_string(b),
                    writes: optional_string(c),
                    error: None,
                })
            } else {
                Some(WorkerMessage::Done { ok: false, value: None, writes: None, error: optional_string(b) })
            }
        }
        _ => None,
    }
}

/// `{ name, message, stack }` JSON of an exception thrown while compiling the script.
fn describe_exception<'js>(ctx: &Ctx<'js>, error: CaughtError<'js>) -> String {
    let (name, message, stack) = match &error {
        CaughtError::Exception(exception) => {
            let object = exception.as_object();
            let name = object.get::<_, Value>("name").ok().as_ref().and_then(js_string).unwrap_or("Error".into());
            (name, exception.message().unwrap_or_default(), exception.stack().unwrap_or_default())
        }
        CaughtError::Value(value) => {
            let text = ctx.json_stringify(value.clone()).ok().flatten().and_then(|s| s.to_string().ok());
            ("Error".into(), text.unwrap_or_default(), String::new())
        }
        CaughtError::Error(error) => ("Error".into(), error.to_string(), String::new()),
    };
    let head = if message.is_empty() { name.clone() } else { format!("{name}: {message}") };
    let stack = stack.trim_end();
    let stack = if stack.is_empty() { head } else { format!("{head}\n{stack}") };
    serde_json::json!({ "name": name, "message": message, "stack": stack }).to_string()
}

fn caught_text(error: CaughtError<'_>) -> String {
    match error {
        CaughtError::Exception(exception) => {
            let name = exception.as_object().get::<_, Value>("name").ok().as_ref().and_then(js_string);
            format!("{}: {}", name.unwrap_or("Error".into()), exception.message().unwrap_or_default())
        }
        other => other.to_string(),
    }
}

fn make_bridge<'js>(ctx: &Ctx<'js>, sender: UnboundedSender<WorkerMessage>) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, kind: String, a: Opt<Value<'js>>, b: Opt<Value<'js>>, c: Opt<Value<'js>>| {
            match bridge_message(&kind, a, b, c) {
                Some(message) => {
                    let _ = sender.send(message);
                    Ok(())
                }
                None => Err(rquickjs::Exception::throw_type(&ctx, "malformed bridge call")),
            }
        },
    )
}

struct Api {
    settle: Persistent<Function<'static>>,
    run: Persistent<Function<'static>>,
    stalled: Persistent<Function<'static>>,
}

fn run(setup: WorkerSetup) -> Result<(), String> {
    let WorkerSetup { code, tools_json, globals_json, store_json, memory_limit, interrupt, to_host, replies } = setup;
    let runtime = Runtime::new().map_err(|error| format!("Failed to create the QuickJS runtime: {error}"))?;
    if let Some(limit) = memory_limit {
        runtime.set_memory_limit(limit);
    }
    runtime.set_max_stack_size(MAX_JS_STACK_BYTES);
    let flag = interrupt.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || flag.load(Ordering::Relaxed))));
    let context = Context::full(&runtime).map_err(|error| format!("Failed to create the QuickJS context: {error}"))?;

    let api = context.with(|ctx| -> Result<Api, String> {
        let bridge = make_bridge(&ctx, to_host.clone()).map_err(|error| error.to_string())?;
        let factory: Function = ctx
            .eval_with_options(PRELUDE_SOURCE, eval_options("codemode-prelude.js"))
            .catch(&ctx)
            .map_err(caught_text)?;
        let api: Object =
            factory.call((bridge, tools_json, globals_json, store_json)).catch(&ctx).map_err(caught_text)?;
        let get = |name: &str| -> Result<Persistent<Function<'static>>, String> {
            let function: Function = api.get(name).map_err(|error| error.to_string())?;
            Ok(Persistent::save(&ctx, function))
        };
        Ok(Api { settle: get("settle")?, run: get("run")?, stalled: get("stalled")? })
    })?;

    // The prefix shares the first line with the script so reported line numbers match it.
    let wrapped = format!("(async (tools, console) => {{{code}\n}})");
    let started = context.with(|ctx| -> Result<bool, String> {
        match ctx.eval_with_options::<Value, _>(wrapped, eval_options("codemode.js")).catch(&ctx) {
            Ok(script) => {
                let run = api.run.clone().restore(&ctx).map_err(|error| error.to_string())?;
                run.call::<_, Value>((script,)).catch(&ctx).map_err(caught_text)?;
                Ok(true)
            }
            Err(error) => {
                let error = describe_exception(&ctx, error);
                let _ = to_host.send(WorkerMessage::Done { ok: false, value: None, writes: None, error: Some(error) });
                Ok(false)
            }
        }
    })?;
    if started {
        drain(&runtime, &context, &api, &interrupt)?;
        while let Ok(reply) = replies.recv() {
            if interrupt.load(Ordering::SeqCst) {
                break;
            }
            context.with(|ctx| -> Result<(), String> {
                let settle = api.settle.clone().restore(&ctx).map_err(|error| error.to_string())?;
                settle.call::<_, Value>((reply.id as f64, reply.ok, reply.payload)).catch(&ctx).map_err(caught_text)?;
                Ok(())
            })?;
            drain(&runtime, &context, &api, &interrupt)?;
        }
    }
    drop(api);
    Ok(())
}

/// Run queued jobs, then fail a script that waits on nothing that can ever resume it.
fn drain(runtime: &Runtime, context: &Context, api: &Api, interrupt: &AtomicBool) -> Result<(), String> {
    loop {
        if interrupt.load(Ordering::Relaxed) {
            return Ok(());
        }
        match runtime.execute_pending_job() {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => {
                return context.with(|ctx| Err(caught_text(CaughtError::from_error(&ctx, rquickjs::Error::Exception))));
            }
        }
    }
    context.with(|ctx| -> Result<(), String> {
        let stalled = api.stalled.clone().restore(&ctx).map_err(|error| error.to_string())?;
        stalled.call::<_, Value>(()).catch(&ctx).map_err(caught_text)?;
        Ok(())
    })
}
