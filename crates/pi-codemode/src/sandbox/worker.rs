//! The QuickJS side of one execution: a dedicated thread with its own runtime, which relays tool
//! calls and output to the host and reports the result. The host stops it with the interrupt flag.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;

use rquickjs::context::EvalOptions;
use rquickjs::function::Opt;
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Function, Object, Persistent, Runtime, Value};
use tokio::sync::mpsc::UnboundedSender;

use super::types::{CodemodeDuring, CodemodeOutputItem};

const PRELUDE_SOURCE: &str = include_str!("prelude.js");
const DETERMINISTIC_SOURCE: &str = include_str!("deterministic.js");
/// The `rquickjs` version in Cargo.toml, which pins the engine; part of the fingerprint.
const ENGINE_VERSION: &str = "0.14.0";
/// Bumped whenever the worker or the scripts change what a deterministic script observes, beyond what
/// the fingerprint's other parts (engine version, script hash, limits) already capture.
const DETERMINISTIC_VERSION: u32 = 1;
/// Native stack of the worker thread, and the part QuickJS may use before it throws a `RangeError`.
const THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
const MAX_JS_STACK_BYTES: usize = 8 * 1024 * 1024;

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
    /// `writes` is a JSON array of `[key, json]` for `store()` and `[key]` for deletions. Deterministic
    /// mode: `during` is the result the VM was processing when the script finished.
    Done {
        ok: bool,
        value: Option<String>,
        writes: Option<String>,
        error: Option<String>,
        during: Option<CodemodeDuring>,
    },
    /// The VM failed outside the script's control.
    Crash(String),
    /// Deterministic mode, before the script runs: what the VM is, so a replay can refuse a different one.
    Started(String),
    /// Deterministic mode, after the script waits again: having processed result `seq` (0: the script's
    /// first run), it has issued `issued` calls in total.
    Idle {
        seq: u64,
        issued: u64,
    },
}

/// A settled host call: `payload` is the JSON result when `ok`, otherwise the error message.
/// Deterministic mode: `seq` numbers the results from 1 in the order the VM applies them, the VM's clock
/// reads `now` while it processes this one, and `properties` (`ok: false`) is a JSON object whose
/// properties the script's `Error` gets.
#[derive(Debug, Default)]
pub(crate) struct HostReply {
    pub id: u64,
    pub ok: bool,
    pub payload: Option<String>,
    pub seq: Option<u64>,
    pub now: Option<f64>,
    pub properties: Option<String>,
}

/// Run deterministically (`CodemodeSandbox::start`): UTC, a virtual clock that starts at `started_at`
/// (ms since the epoch) and moves only with delivered results, randomness from `seed`, no `WeakRef`,
/// and results applied in `seq` order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deterministic {
    pub started_at: f64,
    pub seed: u32,
}

pub(crate) struct WorkerSetup {
    pub code: String,
    pub tools_json: String,
    pub globals_json: String,
    pub store_json: String,
    pub memory_limit: Option<usize>,
    pub interrupt: Arc<AtomicBool>,
    pub deterministic: Option<Deterministic>,
    pub to_host: UnboundedSender<WorkerMessage>,
    pub replies: mpsc::Receiver<HostReply>,
}

struct ReleaseOnDrop(Option<super::WorkerRelease>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            release();
        }
    }
}

/// Start the worker thread. `release` runs when the thread exits, or at once if it fails to start.
pub(crate) fn spawn(setup: WorkerSetup, release: Option<super::WorkerRelease>) -> std::io::Result<()> {
    let release = ReleaseOnDrop(release);
    std::thread::Builder::new().name("codemode".into()).stack_size(THREAD_STACK_BYTES).spawn(move || {
        let _release = release;
        let to_host = setup.to_host.clone();
        let interrupt = setup.interrupt.clone();
        if let Err(message) = run(setup)
            && !interrupt.load(Ordering::SeqCst)
        {
            let _ = to_host.send(WorkerMessage::Crash(message));
        }
    })?;
    Ok(())
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

/// What the bridge counts for the deterministic reports: calls issued, and the result being processed.
#[derive(Clone, Default)]
struct Progress {
    deterministic: bool,
    issued: Arc<AtomicU64>,
    current: Arc<AtomicU64>,
}

impl Progress {
    fn during(&self) -> Option<CodemodeDuring> {
        self.deterministic.then(|| CodemodeDuring {
            seq: self.current.load(Ordering::SeqCst),
            issued: self.issued.load(Ordering::SeqCst),
        })
    }
}

fn bridge_message(
    kind: &str,
    a: Opt<Value<'_>>,
    b: Opt<Value<'_>>,
    c: Opt<Value<'_>>,
    progress: &Progress,
) -> Option<WorkerMessage> {
    match kind {
        "call" | "global" => {
            progress.issued.fetch_add(1, Ordering::SeqCst);
            Some(WorkerMessage::Call {
                id: a.0.as_ref()?.as_number()? as u64,
                target: if kind == "call" { CallTarget::Tool } else { CallTarget::Global },
                name: optional_string(b)?,
                args: optional_string(c),
            })
        }
        "output" => {
            let data = optional_string(b)?;
            Some(WorkerMessage::Output(match optional_string(a)?.as_str() {
                "image" => CodemodeOutputItem::Image { data, mime_type: optional_string(c)? },
                "console" => CodemodeOutputItem::Text { text: data, console: true },
                _ => CodemodeOutputItem::Text { text: data, console: false },
            }))
        }
        "done" => {
            let during = progress.during();
            if a.0.as_ref()?.as_bool()? {
                Some(WorkerMessage::Done {
                    ok: true,
                    value: optional_string(b),
                    writes: optional_string(c),
                    error: None,
                    during,
                })
            } else {
                Some(WorkerMessage::Done { ok: false, value: None, writes: None, error: optional_string(b), during })
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

fn make_bridge<'js>(
    ctx: &Ctx<'js>,
    sender: UnboundedSender<WorkerMessage>,
    progress: Progress,
) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, kind: String, a: Opt<Value<'js>>, b: Opt<Value<'js>>, c: Opt<Value<'js>>| {
            match bridge_message(&kind, a, b, c, &progress) {
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

/// FNV-1a over the bytes, as 8 hex digits.
fn fnv1a(text: &str) -> String {
    let hash = text.bytes().fold(0x811c_9dc5_u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193));
    format!("{hash:08x}")
}

/// Identifies the VM, so a host that replays a run can refuse a different one.
fn fingerprint(memory_limit: Option<usize>) -> String {
    let scripts = fnv1a(&format!("{PRELUDE_SOURCE}\0{DETERMINISTIC_SOURCE}"));
    let heap = memory_limit.map_or_else(|| "none".to_string(), |limit| limit.to_string());
    format!(
        "rquickjs {ENGINE_VERSION}, prelude {scripts}, deterministic {DETERMINISTIC_VERSION}, \
         stack {MAX_JS_STACK_BYTES}, heap {heap}, budget none"
    )
}

/// QuickJS's `Math.random()` stream for a context whose clock read `seed + 1` microseconds at creation:
/// xorshift64* from that state, after the draw QuickJS spends on its hash seed.
fn seeded_random(seed: u32) -> impl Fn() -> f64 {
    fn next(state: &Cell<u64>) -> u64 {
        let mut x = state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    let state = Cell::new(u64::from(seed) + 1);
    next(&state);
    move || f64::from_bits((0x3ff_u64 << 52) | (next(&state) >> 12)) - 1.0
}

/// Replace the clock, `Math.random()`, the timezone, and `WeakRef` before the prelude freezes them.
fn install_determinism<'js>(ctx: &Ctx<'js>, mode: Deterministic, clock: &Arc<AtomicU64>) -> Result<(), String> {
    let now = {
        let clock = clock.clone();
        Function::new(ctx.clone(), move || f64::from_bits(clock.load(Ordering::SeqCst)))
    };
    let random = Function::new(ctx.clone(), seeded_random(mode.seed));
    let (now, random) = (now.map_err(|e| e.to_string())?, random.map_err(|e| e.to_string())?);
    let setup: Function = ctx
        .eval_with_options(DETERMINISTIC_SOURCE, eval_options("codemode-prelude.js"))
        .catch(ctx)
        .map_err(caught_text)?;
    setup.call::<_, Value>((now, random, mode.started_at)).catch(ctx).map_err(caught_text)?;
    Ok(())
}

/// Settle `reply` in the VM and run the jobs it queued.
fn settle(
    runtime: &Runtime,
    context: &Context,
    api: &Api,
    interrupt: &AtomicBool,
    reply: HostReply,
) -> Result<(), String> {
    context.with(|ctx| -> Result<(), String> {
        let settle = api.settle.clone().restore(&ctx).map_err(|error| error.to_string())?;
        let arguments = (reply.id as f64, reply.ok, reply.payload, reply.properties);
        settle.call::<_, Value>(arguments).catch(&ctx).map_err(caught_text)?;
        Ok(())
    })?;
    drain(runtime, context, api, interrupt)
}

fn run(setup: WorkerSetup) -> Result<(), String> {
    let WorkerSetup {
        code,
        tools_json,
        globals_json,
        store_json,
        memory_limit,
        interrupt,
        deterministic,
        to_host,
        replies,
    } = setup;
    let runtime = Runtime::new().map_err(|error| format!("Failed to create the QuickJS runtime: {error}"))?;
    if let Some(limit) = memory_limit {
        runtime.set_memory_limit(limit);
    }
    runtime.set_max_stack_size(MAX_JS_STACK_BYTES);
    let flag = interrupt.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || flag.load(Ordering::Relaxed))));
    let context = Context::full(&runtime).map_err(|error| format!("Failed to create the QuickJS context: {error}"))?;
    let progress = Progress { deterministic: deterministic.is_some(), ..Progress::default() };
    let clock = Arc::new(AtomicU64::new(deterministic.map_or(0.0, |mode| mode.started_at).to_bits()));

    let api = context.with(|ctx| -> Result<Api, String> {
        if let Some(mode) = deterministic {
            install_determinism(&ctx, mode, &clock)?;
        }
        let bridge = make_bridge(&ctx, to_host.clone(), progress.clone()).map_err(|error| error.to_string())?;
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
    if deterministic.is_some() {
        let _ = to_host.send(WorkerMessage::Started(fingerprint(memory_limit)));
    }

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
                let during = progress.during();
                let _ = to_host.send(WorkerMessage::Done {
                    ok: false,
                    value: None,
                    writes: None,
                    error: Some(error),
                    during,
                });
                Ok(false)
            }
        }
    })?;
    if started {
        drain(&runtime, &context, &api, &interrupt)?;
        if deterministic.is_some() {
            let _ = to_host.send(WorkerMessage::Idle { seq: 0, issued: progress.issued.load(Ordering::SeqCst) });
        }
        let mut next_seq = 1_u64;
        let mut early: BTreeMap<u64, HostReply> = BTreeMap::new();
        while let Ok(reply) = replies.recv() {
            if interrupt.load(Ordering::SeqCst) {
                break;
            }
            if deterministic.is_none() {
                settle(&runtime, &context, &api, &interrupt, reply)?;
                continue;
            }
            // Results arrive in any order; the VM applies them strictly by number.
            let Some(seq) = reply.seq.filter(|seq| *seq >= next_seq && !early.contains_key(seq)) else {
                let seq = reply.seq.map_or_else(|| "undefined".to_string(), |seq| seq.to_string());
                return Err(format!(
                    "Result {} has sequence number {seq}, expected {next_seq} or later, once",
                    reply.id
                ));
            };
            early.insert(seq, reply);
            while let Some(reply) = early.remove(&next_seq) {
                // Never backwards: a resumed host's clock may be behind the one that started the run.
                if let Some(now) = reply.now.filter(|now| now.is_finite()) {
                    let previous = f64::from_bits(clock.load(Ordering::SeqCst));
                    clock.store(previous.max(now).to_bits(), Ordering::SeqCst);
                }
                progress.current.store(next_seq, Ordering::SeqCst);
                settle(&runtime, &context, &api, &interrupt, reply)?;
                let issued = progress.issued.load(Ordering::SeqCst);
                let _ = to_host.send(WorkerMessage::Idle { seq: next_seq, issued });
                next_seq += 1;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_version_matches_the_manifest() {
        let manifest = include_str!("../../Cargo.toml");
        assert!(manifest.contains(&format!("rquickjs = {{ version = \"{ENGINE_VERSION}\"")));
    }

    #[test]
    fn hashes_text_with_fnv1a() {
        assert_eq!(fnv1a(""), "811c9dc5");
        assert_eq!(fnv1a("a"), "e40c292c");
    }
}
