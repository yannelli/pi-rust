//! Natives for output, `pi:host`, timers, and `process`.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rquickjs::function::{Opt, Rest};
use rquickjs::{Ctx, Function, Object, Persistent, Value};

use super::set_fn;
use crate::error::JsError;
use crate::runtime::OutputStream;
use crate::runtime::convert::{latin1_to_bytes, throw};
use crate::runtime::scope::call_host;
use crate::runtime::state::state;

/// The Node version extensions see in `process.version`.
pub(crate) const NODE_VERSION: &str = "22.12.0";

fn started() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn stream(name: &str) -> OutputStream {
    if name == "stderr" { OutputStream::Stderr } else { OutputStream::Stdout }
}

pub(crate) fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub(crate) fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        other => other,
    }
}

pub(crate) fn install<'js>(ctx: &Ctx<'js>, native: &Object<'js>) -> rquickjs::Result<()> {
    started();
    let io = Object::new(ctx.clone())?;
    set_fn(&io, "writeText", |name: String, text: String| state().shared.output.write(stream(&name), text.as_bytes()))?;
    set_fn(&io, "writeBytes", |name: String, bytes: String| {
        state().shared.output.write(stream(&name), &latin1_to_bytes(&bytes));
    })?;
    set_fn(&io, "isTTY", |name: String| state().shared.output.is_tty(stream(&name)))?;
    set_fn(&io, "size", || state().shared.output.terminal_size().map(|(columns, rows)| vec![columns, rows]))?;
    native.set("io", io)?;

    let host = Object::new(ctx.clone())?;
    set_fn(&host, "call", |ctx: Ctx<'js>, name: String, args: Rest<Value<'js>>| {
        let function =
            state().shared.host_functions.read().unwrap_or_else(|poison| poison.into_inner()).get(&name).cloned();
        let function = function.ok_or_else(|| {
            throw(&ctx, &JsError::new(format!("Unknown host function: {name}")).with_code("ERR_PI_HOST_FUNCTION"))
        })?;
        call_host(&ctx, &function, args.0)
    })?;
    set_fn(&host, "has", |name: String| {
        state().shared.host_functions.read().unwrap_or_else(|poison| poison.into_inner()).get(&name).is_some()
    })?;
    set_fn(&host, "names", || {
        state().shared.host_functions.read().unwrap_or_else(|poison| poison.into_inner()).names()
    })?;
    native.set("host", host)?;

    let util = Object::new(ctx.clone())?;
    set_fn(&util, "promiseState", |ctx: Ctx<'js>, promise: rquickjs::Promise<'js>| {
        let state = Object::new(ctx.clone())?;
        match promise.state() {
            rquickjs::promise::PromiseState::Pending => state.set("state", "pending")?,
            rquickjs::promise::PromiseState::Resolved => {
                state.set("state", "fulfilled")?;
                state.set("value", promise.result::<Value>().transpose()?)?;
            }
            rquickjs::promise::PromiseState::Rejected => {
                state.set("state", "rejected")?;
                let reason = match promise.result::<Value>() {
                    Some(Err(rquickjs::Error::Exception)) => ctx.catch(),
                    _ => Value::new_undefined(ctx.clone()),
                };
                state.set("value", reason)?;
            }
        }
        Ok::<_, rquickjs::Error>(state)
    })?;
    set_fn(&util, "isProxy", |value: Value<'js>| value.is_proxy())?;
    native.set("util", util)?;
    native.set("timers", timers(ctx)?)?;
    native.set("process", process(ctx)?)?;
    Ok(())
}

fn timers<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let timers = Object::new(ctx.clone())?;
    set_fn(&timers, "start", |ctx: Ctx<'js>, callback: Function<'js>, delay: f64, repeat: bool| {
        let state = state();
        let id = state.next_id();
        let delay = Duration::from_secs_f64(if delay.is_finite() && delay > 0.0 { delay / 1000.0 } else { 0.0 });
        state.timers.borrow_mut().insert(id, Persistent::save(&ctx, callback), delay, repeat);
        id
    })?;
    set_fn(&timers, "clear", |id: u64| state().timers.borrow_mut().remove(id))?;
    set_fn(&timers, "ref", |id: u64, referenced: bool| state().timers.borrow_mut().set_referenced(id, referenced))?;
    set_fn(&timers, "hasRef", |id: u64| state().timers.borrow().is_referenced(id))?;
    set_fn(&timers, "refresh", |id: u64| state().timers.borrow_mut().refresh(id))?;
    set_fn(&timers, "immediate", |ctx: Ctx<'js>, callback: Function<'js>| {
        let state = state();
        let id = state.next_id();
        state.immediates.borrow_mut().push_back((id, Persistent::save(&ctx, callback)));
        id
    })?;
    set_fn(&timers, "clearImmediate", |id: u64| state().immediates.borrow_mut().retain(|(known, _)| *known != id))?;
    Ok(timers)
}

fn process<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let process = Object::new(ctx.clone())?;
    set_fn(&process, "cwd", || state().cwd.borrow().display().to_string())?;
    set_fn(&process, "chdir", |ctx: Ctx<'js>, dir: String| {
        let state = state();
        let target = state.cwd.borrow().join(&dir);
        if !target.is_dir() {
            let error = std::io::Error::from(std::io::ErrorKind::NotFound);
            return Err(throw(&ctx, &JsError::from_io(&error, "chdir", Some(&dir))));
        }
        *state.cwd.borrow_mut() = crate::resolve::canonical(&target);
        Ok(())
    })?;
    set_fn(&process, "hrtime", || {
        let elapsed = started().elapsed();
        vec![elapsed.as_secs() as f64, elapsed.subsec_nanos() as f64]
    })?;
    set_fn(&process, "uptime", || started().elapsed().as_secs_f64())?;
    set_fn(&process, "scheduleTicks", || state().ticks_pending.set(true))?;
    set_fn(&process, "scheduleIdle", || state().idle_hooks.set(true))?;
    set_fn(&process, "exit", |code: Opt<i32>| exit(code.0.unwrap_or(0)))?;
    set_fn(&process, "kill", |ctx: Ctx<'js>, pid: i64, signal: Opt<String>| {
        kill(pid, signal.0.as_deref().unwrap_or("SIGTERM")).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(&process, "rss", resident_memory)?;
    set_fn(&process, "abort", abort)?;
    set_fn(&process, "loopTimes", || {
        let state = state();
        let start = state.loop_start.get().map_or(0.0, |start| start.duration_since(started()).as_secs_f64() * 1000.0);
        vec![start, state.idle_time.get().as_secs_f64() * 1000.0]
    })?;
    set_fn(&process, "uid", |name: String| user_id(&name))?;
    Ok(process)
}

/// `process.abort`: end the process at once, as Node's abort() does.
fn abort() {
    std::process::abort()
}

/// `process.exit`: hand the code to the embedder (or exit), then stop running JavaScript.
fn exit(code: i32) {
    let state = state();
    match &state.shared.config.on_exit {
        Some(callback) => callback(code),
        None => std::process::exit(code),
    }
    state.shutting_down.set(true);
    state.shared.interrupt.store(true, Ordering::SeqCst);
}

pub(crate) fn kill(pid: i64, signal: &str) -> Result<bool, JsError> {
    let signal = signal.strip_prefix("SIG").unwrap_or(signal);
    #[cfg(unix)]
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status();
    #[cfg(windows)]
    let status = {
        let _ = signal;
        std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).status()
    };
    match status {
        Ok(status) if status.success() => Ok(true),
        Ok(_) => Err(JsError::new("kill ESRCH").with_code("ESRCH").with_data("syscall", "kill")),
        Err(error) => Err(JsError::from_io(&error, "kill", None)),
    }
}

/// Resident set size in bytes where the platform reports it cheaply, else 0.
fn resident_memory() -> f64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else { return 0.0 };
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb * 1024.0)
}

/// `id -u` style lookups for `process.getuid()` and friends; `None` where unavailable.
fn user_id(kind: &str) -> Option<u32> {
    if !cfg!(unix) {
        return None;
    }
    let flag = match kind {
        "uid" => "-u",
        "gid" => "-g",
        "euid" => "-u",
        "egid" => "-g",
        _ => return None,
    };
    let output = std::process::Command::new("id").arg(flag).output().ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Startup values for `process` and `os`.
pub(crate) fn config<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let state = state();
    let config = &state.shared.config;
    let object = Object::new(ctx.clone())?;
    object.set("cwd", config.cwd.display().to_string())?;
    object.set("argv", config.argv.clone())?;
    let env = Object::new(ctx.clone())?;
    for (key, value) in &config.env {
        env.set(key.as_str(), value.as_str())?;
    }
    object.set("env", env)?;
    let exec_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pi"));
    object.set("execPath", exec_path.display().to_string())?;
    object.set("platform", platform())?;
    object.set("arch", arch())?;
    object.set("processStdio", state.shared.output.is_process_stdio())?;
    object.set("pid", std::process::id())?;
    #[cfg(unix)]
    object.set("ppid", std::os::unix::process::parent_id())?;
    #[cfg(not(unix))]
    object.set("ppid", 0)?;
    object.set("nodeVersion", NODE_VERSION)?;
    object.set("piJsVersion", env!("CARGO_PKG_VERSION"))?;
    Ok(object)
}
