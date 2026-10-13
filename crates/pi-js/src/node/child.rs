//! Natives for `node:child_process`: tokio children whose output and exit are posted back to a
//! JavaScript callback, and blocking `spawnSync` with input and a deadline.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Object, Persistent, Value};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_to_js, error_value, latin1_to_bytes, throw, to_json};
use crate::runtime::scope::spawn_op;
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpawnOptions {
    file: String,
    args: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    /// `pipe` or `ignore` for stdin, stdout, and stderr.
    stdio: [String; 3],
    #[serde(default)]
    #[cfg_attr(not(windows), allow(dead_code))]
    windows_hide: bool,
    #[serde(default)]
    #[cfg_attr(not(windows), allow(dead_code))]
    windows_verbatim_arguments: bool,
    #[serde(default)]
    #[cfg_attr(not(unix), allow(dead_code))]
    detached: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    argv0: Option<String>,
    #[cfg_attr(not(unix), allow(dead_code))]
    uid: Option<u32>,
    #[cfg_attr(not(unix), allow(dead_code))]
    gid: Option<u32>,
}

fn stdio(kind: &str) -> Stdio {
    if kind == "pipe" { Stdio::piped() } else { Stdio::null() }
}

fn build(options: &SpawnOptions) -> Command {
    let mut command = Command::new(&options.file);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if options.windows_verbatim_arguments {
            for arg in &options.args {
                command.raw_arg(arg);
            }
        } else {
            command.args(&options.args);
        }
        if options.windows_hide {
            command.creation_flags(0x0800_0000);
        }
    }
    #[cfg(not(windows))]
    command.args(&options.args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if let Some(argv0) = &options.argv0 {
            command.arg0(argv0);
        }
        if let Some(uid) = options.uid {
            command.uid(uid);
        }
        if let Some(gid) = options.gid {
            command.gid(gid);
        }
        if options.detached {
            command.process_group(0);
        }
    }
    command.current_dir(&options.cwd).env_clear().envs(options.env.iter().map(|(key, value)| (key, value)));
    command.stdin(stdio(&options.stdio[0])).stdout(stdio(&options.stdio[1])).stderr(stdio(&options.stdio[2]));
    command
}

/// Node's spawn failure: `spawn <file> ENOENT` with `path` and `spawnargs`.
fn spawn_error(error: &std::io::Error, options: &SpawnOptions) -> JsError {
    let (code, _) = crate::error::io_code(error);
    let syscall = format!("spawn {}", options.file);
    let mut result = JsError::new(format!("{syscall} {code}")).with_code(code).with_data("syscall", syscall);
    if let Some(errno) = error.raw_os_error() {
        result = result.with_data("errno", -errno);
    }
    result.with_data("path", options.file.clone()).with_data("spawnargs", options.args.clone())
}

/// JavaScript-thread bookkeeping for one child.
struct ChildState {
    pid: u32,
    stdin: Option<Arc<tokio::sync::Mutex<Option<ChildStdin>>>>,
    /// Readers and the exit watcher still to report; the callback is released at zero.
    remaining: u8,
    /// Output readers still running (stdout, stderr), so JavaScript can stop one early.
    outputs: [Option<tokio::task::AbortHandle>; 2],
    referenced: bool,
    exited: bool,
    #[cfg(windows)]
    kill: tokio::sync::mpsc::UnboundedSender<()>,
}

enum Event {
    Data(&'static str, Vec<u8>),
    End(&'static str),
    Exit(Option<i32>, Option<i32>),
}

fn post(sender: &std::sync::mpsc::Sender<Message>, id: u64, event: Event) {
    let task: Task = Box::new(move |ctx| deliver(ctx, id, event));
    let _ = sender.send(Message::Task(task));
}

fn output_index(name: &str) -> usize {
    usize::from(name == "stderr")
}

/// Count one finished source; at zero, release the callback and the loop reference.
fn finish_source(id: u64) {
    let state = state();
    let done = state
        .with_resource(id, |child: &mut ChildState| {
            child.remaining -= 1;
            (child.remaining == 0).then_some(child.referenced)
        })
        .ok()
        .flatten();
    if let Some(referenced) = done {
        state.resources.borrow_mut().remove(&id);
        state.callbacks.borrow_mut().remove(&id);
        if referenced {
            state.remove_busy();
        }
    }
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let state = state();
    let finished = match &event {
        Event::Data(..) => false,
        Event::Exit(..) => state.with_resource(id, |child: &mut ChildState| child.exited = true).is_ok(),
        // An output JavaScript already closed was counted then.
        Event::End(name) => state
            .with_resource(id, |child: &mut ChildState| child.outputs[output_index(name)].take().is_some())
            .unwrap_or(false),
    };
    if matches!(event, Event::End(_)) && !finished {
        return;
    }
    let callback = state.callbacks.borrow().get(&id).cloned();
    if let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) {
        let result = match event {
            Event::Data(name, bytes) => bytes_to_js(ctx, bytes).and_then(|bytes| function.call::<_, ()>((name, bytes))),
            Event::End(name) => function.call::<_, ()>((format!("{name}-end"),)),
            Event::Exit(code, signal) => function.call::<_, ()>(("exit", code, signal)),
        };
        if let Err(error) = result {
            report_error(ctx, UncaughtKind::Exception, error);
        }
    }
    if finished {
        finish_source(id);
    }
}

async fn read_stream(
    sender: std::sync::mpsc::Sender<Message>,
    id: u64,
    name: &'static str,
    mut stream: impl AsyncRead + Unpin + Send + 'static,
) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => post(&sender, id, Event::Data(name, buffer[..count].to_vec())),
        }
    }
    post(&sender, id, Event::End(name));
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

fn spawn<'js>(ctx: Ctx<'js>, options: Value<'js>, callback: Function<'js>) -> rquickjs::Result<Object<'js>> {
    let options: SpawnOptions = parse(&ctx, options)?;
    let result = Object::new(ctx.clone())?;
    let state = state();
    let mut command = tokio::process::Command::from(build(&options));
    let spawned = {
        let _guard = state.shared.tokio.enter();
        command.spawn()
    };
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            result.set("error", error_value(&ctx, &spawn_error(&error, &options))?)?;
            return Ok(result);
        }
    };
    let id = state.next_id();
    let pid = child.id().unwrap_or(0);
    let sender = state.sender.clone();
    let tokio = &state.shared.tokio;
    let mut remaining = 1;
    let mut outputs = [None, None];
    if let Some(stdout) = child.stdout.take() {
        remaining += 1;
        outputs[0] = Some(tokio.spawn(read_stream(sender.clone(), id, "stdout", stdout)).abort_handle());
    }
    if let Some(stderr) = child.stderr.take() {
        remaining += 1;
        outputs[1] = Some(tokio.spawn(read_stream(sender.clone(), id, "stderr", stderr)).abort_handle());
    }
    let stdin = child.stdin.take().map(|stdin| Arc::new(tokio::sync::Mutex::new(Some(stdin))));
    #[cfg(windows)]
    let (kill, mut kill_requests) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio.spawn(async move {
        #[cfg(windows)]
        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                Some(()) = kill_requests.recv() => { let _ = child.start_kill(); }
            }
        };
        #[cfg(not(windows))]
        let status = child.wait().await;
        let (code, signal) = match status {
            Ok(status) => (status.code(), exit_signal(&status)),
            Err(_) => (None, None),
        };
        post(&sender, id, Event::Exit(code, signal));
    });
    let child_state = ChildState {
        pid,
        stdin,
        remaining,
        outputs,
        referenced: true,
        exited: false,
        #[cfg(windows)]
        kill,
    };
    state.resources.borrow_mut().insert(id, Box::new(child_state));
    state.callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
    state.add_busy();
    result.set("id", id)?;
    result.set("pid", pid)?;
    Ok(result)
}

fn stdin_of(id: u64) -> Result<Arc<tokio::sync::Mutex<Option<ChildStdin>>>, JsError> {
    state()
        .with_resource(id, |child: &mut ChildState| child.stdin.clone())?
        .ok_or_else(|| JsError::new("This socket has been ended by the other party").with_code("EPIPE"))
}

fn kill(id: u64, signal: &str) -> Result<bool, JsError> {
    let state = state();
    let Ok((pid, exited)) = state.with_resource(id, |child: &mut ChildState| (child.pid, child.exited)) else {
        return Ok(false);
    };
    if exited {
        return Ok(false);
    }
    #[cfg(windows)]
    {
        let _ = signal;
        let _ = pid;
        state.with_resource(id, |child: &mut ChildState| child.kill.send(()).is_ok())
    }
    #[cfg(not(windows))]
    super::core::kill(i64::from(pid), signal)
}

fn parse<'js, T: serde::de::DeserializeOwned>(ctx: &Ctx<'js>, value: Value<'js>) -> rquickjs::Result<T> {
    let json = to_json(ctx, value).map_err(|error| throw(ctx, &error))?;
    serde_json::from_value(json).map_err(|error| throw(ctx, &JsError::type_error(format!("Invalid options: {error}"))))
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "spawn", spawn)?;
    set_fn(&object, "write", |ctx: Ctx<'js>, id: u64, data: String| {
        let stdin = stdin_of(id).map_err(|error| throw(&ctx, &error))?;
        let bytes = latin1_to_bytes(&data);
        let work = async move {
            let mut guard = stdin.lock().await;
            let pipe = guard.as_mut().ok_or_else(|| JsError::new("write after end").with_code("EPIPE"))?;
            pipe.write_all(&bytes).await.map_err(|error| JsError::from_io(&error, "write", None))?;
            pipe.flush().await.map_err(|error| JsError::from_io(&error, "write", None))
        };
        spawn_op(&ctx, work, |ctx, ()| Ok(Value::new_undefined(ctx.clone())))
    })?;
    set_fn(&object, "end", |ctx: Ctx<'js>, id: u64| {
        let stdin = stdin_of(id).map_err(|error| throw(&ctx, &error))?;
        let work = async move {
            stdin.lock().await.take();
            Ok(())
        };
        spawn_op(&ctx, work, |ctx, ()| Ok(Value::new_undefined(ctx.clone())))
    })?;
    set_fn(&object, "closeOutput", |id: u64, name: String| {
        let reader = state().with_resource(id, |child: &mut ChildState| child.outputs[output_index(&name)].take());
        if let Ok(Some(reader)) = reader {
            reader.abort();
            finish_source(id);
        }
    })?;
    set_fn(&object, "kill", |ctx: Ctx<'js>, id: u64, signal: Opt<String>| {
        kill(id, signal.0.as_deref().unwrap_or("SIGTERM")).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(&object, "ref", |id: u64, referenced: bool| {
        let state = state();
        let changed = state.with_resource(id, |child: &mut ChildState| {
            let changed = child.referenced != referenced;
            child.referenced = referenced;
            changed
        });
        if changed.unwrap_or(false) {
            if referenced { state.add_busy() } else { state.remove_busy() }
        }
    })?;
    set_fn(&object, "spawnSync", spawn_sync)?;
    Ok(object)
}

/// Stop a child that ran past its deadline: with `signal` on Unix, by termination elsewhere.
#[cfg(unix)]
fn terminate(child: &mut std::process::Child, signal: &str) {
    let _ = super::core::kill(i64::from(child.id()), signal);
}

#[cfg(not(unix))]
fn terminate(child: &mut std::process::Child, _signal: &str) {
    let _ = child.kill();
}

fn spawn_sync<'js>(
    ctx: Ctx<'js>,
    options: Value<'js>,
    input: Opt<Option<String>>,
    timeout: Opt<Option<f64>>,
    kill_signal: Opt<String>,
) -> rquickjs::Result<Object<'js>> {
    let options: SpawnOptions = parse(&ctx, options)?;
    let result = Object::new(ctx.clone())?;
    let input = input.0.flatten().map(|text| latin1_to_bytes(&text));
    let mut command = build(&options);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            result.set("error", error_value(&ctx, &spawn_error(&error, &options))?)?;
            return Ok(result);
        }
    };
    result.set("pid", child.id())?;
    let writer = child.stdin.take().map(|mut stdin| {
        let input = input.unwrap_or_default();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&input);
        })
    });
    let reader = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = pipe.read_to_end(&mut bytes);
                bytes
            })
        })
    };
    let stdout = reader(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>));
    let stderr = reader(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>));
    let deadline =
        timeout.0.flatten().filter(|ms| *ms > 0.0).map(|ms| Instant::now() + Duration::from_secs_f64(ms / 1000.0));
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if deadline.is_some_and(|deadline| Instant::now() >= deadline) && !timed_out => {
                timed_out = true;
                terminate(&mut child, kill_signal.0.as_deref().unwrap_or("SIGTERM"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => break Err(error),
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let collect =
        |handle: Option<std::thread::JoinHandle<Vec<u8>>>| handle.map(|handle| handle.join().unwrap_or_default());
    if let Some(bytes) = collect(stdout) {
        result.set("stdout", bytes_to_js(&ctx, bytes)?)?;
    }
    if let Some(bytes) = collect(stderr) {
        result.set("stderr", bytes_to_js(&ctx, bytes)?)?;
    }
    match status {
        Ok(status) => {
            result.set("status", status.code())?;
            result.set("signal", exit_signal(&status))?;
        }
        Err(error) => result.set("error", error_value(&ctx, &JsError::from_io(&error, "spawnSync", None))?)?,
    }
    result.set("timedOut", timed_out)?;
    Ok(result)
}
