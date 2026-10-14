//! Byte streams read on tokio and delivered to a JavaScript callback: process stdin and the
//! IPC channel between a forked child and its parent (a Unix socket or a Windows named pipe).

use std::io::Read;
use std::sync::Arc;

use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Object, Persistent, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_to_js, error_value, latin1_to_bytes, throw};
use crate::runtime::scope::spawn_op;
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;
type Writer = Arc<tokio::sync::Mutex<Option<BoxWrite>>>;

struct Pipe {
    referenced: bool,
    reader: tokio::task::AbortHandle,
    writer: Writer,
}

enum Event {
    Connect,
    Data(Vec<u8>),
    End,
    Error(JsError),
}

fn post(sender: &std::sync::mpsc::Sender<Message>, id: u64, event: Event) {
    let task: Task = Box::new(move |ctx| deliver(ctx, id, event));
    let _ = sender.send(Message::Task(task));
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let state = state();
    if state.with_resource(id, |_: &mut Pipe| ()).is_err() {
        return;
    }
    let callback = state.callbacks.borrow().get(&id).cloned();
    let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else { return };
    let result = match event {
        Event::Connect => function.call::<_, ()>(("connect",)),
        Event::Data(bytes) => bytes_to_js(ctx, bytes).and_then(|bytes| function.call::<_, ()>(("data", bytes))),
        Event::End => function.call::<_, ()>(("end",)),
        Event::Error(error) => error_value(ctx, &error).and_then(|error| function.call::<_, ()>(("error", error))),
    };
    if let Err(error) = result {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

/// Start reading what `connect` yields; writes wait until it has connected.
fn open<'js, F>(ctx: &Ctx<'js>, callback: Function<'js>, connect: F) -> rquickjs::Result<u64>
where
    F: std::future::Future<Output = std::io::Result<(BoxRead, Option<BoxWrite>)>> + Send + 'static,
{
    let state = state();
    let id = state.next_id();
    let sender = state.sender.clone();
    let writer: Writer = Arc::new(tokio::sync::Mutex::new(None));
    let mut guard = writer.clone().try_lock_owned().map_err(|_| throw(ctx, &JsError::new("pipe writer is busy")))?;
    let reader = state.shared.tokio.spawn(async move {
        let (mut read, write) = match connect.await {
            Ok(pair) => pair,
            Err(error) => {
                drop(guard);
                post(&sender, id, Event::Error(JsError::from_io(&error, "connect", None)));
                return;
            }
        };
        *guard = write;
        drop(guard);
        post(&sender, id, Event::Connect);
        let mut buffer = vec![0; 64 * 1024];
        loop {
            match read.read(&mut buffer).await {
                Ok(0) => break,
                Ok(count) => post(&sender, id, Event::Data(buffer[..count].to_vec())),
                Err(error) => {
                    post(&sender, id, Event::Error(JsError::from_io(&error, "read", None)));
                    break;
                }
            }
        }
        post(&sender, id, Event::End);
    });
    let pipe = Pipe { referenced: false, reader: reader.abort_handle(), writer };
    state.resources.borrow_mut().insert(id, Box::new(pipe));
    state.callbacks.borrow_mut().insert(id, Persistent::save(ctx, callback));
    Ok(id)
}

fn set_ref(id: u64, referenced: bool) {
    let state = state();
    let changed = state.with_resource(id, |pipe: &mut Pipe| std::mem::replace(&mut pipe.referenced, referenced));
    match changed {
        Ok(previous) if previous != referenced && referenced => state.add_busy(),
        Ok(previous) if previous != referenced => state.remove_busy(),
        _ => {}
    }
}

fn close(id: u64) {
    set_ref(id, false);
    let state = state();
    let Some(resource) = state.resources.borrow_mut().remove(&id) else { return };
    state.callbacks.borrow_mut().remove(&id);
    if let Ok(pipe) = resource.downcast::<Pipe>() {
        pipe.reader.abort();
        let writer = pipe.writer;
        state.shared.tokio.spawn(async move {
            if let Some(mut write) = writer.lock().await.take() {
                let _ = write.shutdown().await;
            }
        });
    }
}

/// A fresh IPC endpoint name: a socket path on Unix, a pipe name on Windows.
fn endpoint() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "pi-js-ipc-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.subsec_nanos())
    );
    if cfg!(windows) { format!(r"\\.\pipe\{unique}") } else { std::env::temp_dir().join(unique).display().to_string() }
}

#[cfg(unix)]
async fn accept(listener: tokio::net::UnixListener, path: String) -> std::io::Result<(BoxRead, Option<BoxWrite>)> {
    let accepted = listener.accept().await;
    let _ = std::fs::remove_file(&path);
    let (read, write) = accepted?.0.into_split();
    Ok((Box::new(read), Some(Box::new(write))))
}

#[cfg(unix)]
async fn connect(path: String) -> std::io::Result<(BoxRead, Option<BoxWrite>)> {
    let (read, write) = tokio::net::UnixStream::connect(path).await?.into_split();
    Ok((Box::new(read), Some(Box::new(write))))
}

#[cfg(windows)]
async fn accept(
    server: tokio::net::windows::named_pipe::NamedPipeServer,
    _path: String,
) -> std::io::Result<(BoxRead, Option<BoxWrite>)> {
    server.connect().await?;
    let (read, write) = tokio::io::split(server);
    Ok((Box::new(read), Some(Box::new(write))))
}

#[cfg(windows)]
async fn connect(path: String) -> std::io::Result<(BoxRead, Option<BoxWrite>)> {
    let client = tokio::net::windows::named_pipe::ClientOptions::new().open(path)?;
    let (read, write) = tokio::io::split(client);
    Ok((Box::new(read), Some(Box::new(write))))
}

fn listen<'js>(ctx: Ctx<'js>, callback: Function<'js>) -> rquickjs::Result<Object<'js>> {
    let path = endpoint();
    let state = state();
    let _guard = state.shared.tokio.enter();
    #[cfg(unix)]
    let server = tokio::net::UnixListener::bind(&path);
    #[cfg(windows)]
    let server = tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).create(&path);
    let server = server.map_err(|error| throw(&ctx, &JsError::from_io(&error, "listen", Some(&path))))?;
    let id = open(&ctx, callback, accept(server, path.clone()))?;
    let result = Object::new(ctx)?;
    result.set("id", id)?;
    result.set("path", path)?;
    Ok(result)
}

/// Blocking reads of the real stdin for `fs.readSync(0)` and `fs.readFileSync(0)`.
fn read_stdin_sync(ctx: &Ctx<'_>, length: Option<usize>) -> rquickjs::Result<Vec<u8>> {
    let mut stdin = std::io::stdin().lock();
    let result = match length {
        Some(length) => {
            let mut buffer = vec![0; length];
            stdin.read(&mut buffer).map(|count| {
                buffer.truncate(count);
                buffer
            })
        }
        None => {
            let mut buffer = Vec::new();
            stdin.read_to_end(&mut buffer).map(|_| buffer)
        }
    };
    result.map_err(|error| throw(ctx, &JsError::from_io(&error, "read", None)))
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "stdin", |ctx: Ctx<'js>, callback: Function<'js>| {
        open(&ctx, callback, async { Ok((Box::new(tokio::io::stdin()) as BoxRead, None)) })
    })?;
    set_fn(&object, "listen", listen)?;
    set_fn(&object, "connect", |ctx: Ctx<'js>, path: String, callback: Function<'js>| {
        open(&ctx, callback, connect(path))
    })?;
    set_fn(&object, "write", |ctx: Ctx<'js>, id: u64, data: String| {
        let writer =
            state().with_resource(id, |pipe: &mut Pipe| pipe.writer.clone()).map_err(|error| throw(&ctx, &error))?;
        let bytes = latin1_to_bytes(&data);
        let work = async move {
            let mut guard = writer.lock().await;
            let pipe = guard.as_mut().ok_or_else(|| JsError::new("write EPIPE").with_code("EPIPE"))?;
            pipe.write_all(&bytes).await.map_err(|error| JsError::from_io(&error, "write", None))?;
            pipe.flush().await.map_err(|error| JsError::from_io(&error, "write", None))
        };
        spawn_op(&ctx, work, |ctx, ()| Ok(Value::new_undefined(ctx.clone())))
    })?;
    set_fn(&object, "close", close)?;
    set_fn(&object, "ref", set_ref)?;
    set_fn(&object, "readStdinSync", |ctx: Ctx<'js>, length: Opt<usize>| {
        read_stdin_sync(&ctx, length.0).and_then(|bytes| bytes_to_js(&ctx, bytes))
    })?;
    set_fn(&object, "stdinIsTTY", || std::io::IsTerminal::is_terminal(&std::io::stdin()))?;
    set_fn(&object, "setRawMode", |ctx: Ctx<'js>, raw: bool| {
        super::sys::set_raw_mode(raw).map_err(|error| throw(&ctx, &super::os::system_error(&error, "uv_tty_set_mode")))
    })?;
    Ok(object)
}
