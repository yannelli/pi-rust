//! Natives for `node:net` (and the transport under `node:tls`): TCP sockets, Unix sockets or
//! Windows named pipes on tokio. Events reach a JavaScript callback.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rquickjs::{Ctx, Function, Object, Persistent, TypedArray, Value};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::watch;

use super::set_fn;
use super::tls::Tls;
use crate::error::{JsError, io_code};
use crate::runtime::convert::{bytes_of, bytes_to_js, error_value, from_json, throw, to_json};
use crate::runtime::scope::spawn_op;
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(crate) type BoxIo = Box<dyn Io>;
type Writer = Arc<tokio::sync::Mutex<Option<WriteHalf<BoxIo>>>>;
pub(super) type Sender = std::sync::mpsc::Sender<Message>;

/// An open connection before it is attached to a JavaScript socket.
pub(super) struct Conn {
    io: BoxIo,
    tcp: Option<socket2::Socket>,
    info: serde_json::Value,
}

pub(super) enum Kind {
    Stream {
        pending: Option<ReadHalf<BoxIo>>,
        reader: Option<tokio::task::AbortHandle>,
        writer: Writer,
        paused: watch::Sender<bool>,
        tcp: Option<socket2::Socket>,
    },
    Listener {
        path: Option<String>,
        /// A TCP listener's socket, kept so it can be sent to another process.
        socket: Option<socket2::Socket>,
    },
}

pub(super) struct Entry {
    referenced: bool,
    pub(super) task: Option<tokio::task::AbortHandle>,
    kind: Kind,
}

pub(super) enum Event {
    Connect(Conn),
    Connection(Conn),
    Data(Vec<u8>),
    End,
    Error(JsError),
    ClientError(JsError),
}

/// Node's network error: `connect ECONNREFUSED 127.0.0.1:80`, with `address` and `port`.
pub(crate) fn net_error(error: &std::io::Error, syscall: &str, address: Option<&str>, port: Option<u16>) -> JsError {
    let (code, description) = io_code(error);
    let mut message = if syscall == "listen" || syscall == "bind" {
        format!("{syscall} {code}: {description}")
    } else {
        format!("{syscall} {code}")
    };
    match (address, port) {
        (Some(address), Some(port)) => message.push_str(&format!(" {address}:{port}")),
        (Some(address), None) => message.push_str(&format!(" {address}")),
        _ => {}
    }
    let errno = error.raw_os_error().map_or(0, |errno| -errno);
    let mut result = JsError::new(message).with_code(code).with_data("errno", errno).with_data("syscall", syscall);
    if let Some(address) = address {
        result = result.with_data("address", address);
    }
    if let Some(port) = port {
        result = result.with_data("port", port);
    }
    result
}

pub(crate) fn address_json(address: Option<SocketAddr>) -> serde_json::Value {
    match address {
        Some(address) => json!({
            "address": address.ip().to_string(),
            "port": address.port(),
            "family": if address.is_ipv4() { "IPv4" } else { "IPv6" },
        }),
        None => serde_json::Value::Null,
    }
}

pub(super) fn tcp_conn(stream: tokio::net::TcpStream) -> std::io::Result<Conn> {
    let tcp = socket2::SockRef::from(&stream).try_clone()?;
    let info =
        json!({ "local": address_json(stream.local_addr().ok()), "remote": address_json(stream.peer_addr().ok()) });
    Ok(Conn { io: Box::new(stream), tcp: Some(tcp), info })
}

pub(super) fn pipe_conn(io: BoxIo, path: &str) -> Conn {
    Conn { io, tcp: None, info: json!({ "path": path }) }
}

pub(super) async fn secure(conn: Result<Conn, JsError>, tls: Option<Tls>) -> Result<Conn, JsError> {
    let conn = conn?;
    let Some(tls) = tls else { return Ok(conn) };
    let (io, extra) = tls.handshake(conn.io).await?;
    let mut info = conn.info;
    info["tls"] = extra;
    Ok(Conn { io, tcp: conn.tcp, info })
}

pub(super) fn post(sender: &Sender, id: u64, event: Event) {
    let task: Task = Box::new(move |ctx| deliver(ctx, id, event));
    let _ = sender.send(Message::Task(task));
}

pub(super) fn insert<'js>(ctx: &Ctx<'js>, kind: Kind, callback: Option<Function<'js>>) -> u64 {
    let state = state();
    let id = state.next_id();
    state.add_busy();
    state.resources.borrow_mut().insert(id, Box::new(Entry { referenced: true, task: None, kind }));
    if let Some(callback) = callback {
        state.callbacks.borrow_mut().insert(id, Persistent::save(ctx, callback));
    }
    id
}

fn stream_kind(conn: Option<Conn>) -> (Kind, serde_json::Value) {
    let (pending, writer, tcp, info) = match conn {
        Some(conn) => {
            let (read, write) = tokio::io::split(conn.io);
            (Some(read), Some(write), conn.tcp, conn.info)
        }
        None => (None, None, None, serde_json::Value::Null),
    };
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    (Kind::Stream { pending, reader: None, writer, paused: watch::Sender::new(false), tcp }, info)
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let state = state();
    let callback = state.callbacks.borrow().get(&id).cloned();
    let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else { return };
    let result = match event {
        Event::Connect(conn) => {
            let (kind, info) = stream_kind(Some(conn));
            if state.with_resource(id, |entry: &mut Entry| entry.kind = kind).is_err() {
                return;
            }
            start(id);
            from_json(ctx, &info).and_then(|info| function.call::<_, ()>(("connect", info)))
        }
        Event::Connection(conn) => {
            let (kind, info) = stream_kind(Some(conn));
            let child = insert(ctx, kind, None);
            from_json(ctx, &info).and_then(|info| function.call::<_, ()>(("connection", info, child)))
        }
        Event::Data(bytes) => bytes_to_js(ctx, bytes).and_then(|bytes| function.call::<_, ()>(("data", bytes))),
        Event::End => function.call::<_, ()>(("end",)),
        Event::Error(error) => error_value(ctx, &error).and_then(|error| function.call::<_, ()>(("error", error))),
        Event::ClientError(error) => {
            error_value(ctx, &error).and_then(|error| function.call::<_, ()>(("clientError", error)))
        }
    };
    if let Err(error) = result {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

/// A copy of the socket behind a TCP stream or listener, for handle passing.
pub(super) fn socket_of(id: u64) -> Option<std::io::Result<socket2::Socket>> {
    let result = state().with_resource(id, |entry: &mut Entry| match &entry.kind {
        Kind::Stream { tcp: Some(socket), .. } | Kind::Listener { socket: Some(socket), .. } => {
            Some(socket.try_clone())
        }
        _ => None,
    });
    result.ok().flatten()
}

/// Register an open connection that no listener produced; reading starts when JavaScript attaches.
pub(super) fn adopt(ctx: &Ctx<'_>, conn: Conn) -> (u64, serde_json::Value) {
    let (kind, info) = stream_kind(Some(conn));
    (insert(ctx, kind, None), info)
}

/// Begin reading a stream whose callback is set; reads wait while it is paused.
fn start(id: u64) {
    let state = state();
    let sender = state.sender.clone();
    let tokio = state.shared.tokio.clone();
    let _ = state.with_resource(id, |entry: &mut Entry| {
        let Kind::Stream { pending, reader, paused, .. } = &mut entry.kind else { return };
        let Some(mut read) = pending.take() else { return };
        let mut paused = paused.subscribe();
        let task = tokio.spawn(async move {
            let mut buffer = vec![0; 64 * 1024];
            loop {
                while *paused.borrow_and_update() {
                    if paused.changed().await.is_err() {
                        return;
                    }
                }
                match read.read(&mut buffer).await {
                    Ok(0) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Ok(count) => post(&sender, id, Event::Data(buffer[..count].to_vec())),
                    Err(error) => return post(&sender, id, Event::Error(net_error(&error, "read", None, None))),
                }
            }
            post(&sender, id, Event::End);
        });
        *reader = Some(task.abort_handle());
    });
}

pub(super) fn set_ref(id: u64, referenced: bool) {
    let state = state();
    let changed = state.with_resource(id, |entry: &mut Entry| std::mem::replace(&mut entry.referenced, referenced));
    match changed {
        Ok(previous) if previous != referenced && referenced => state.add_busy(),
        Ok(previous) if previous != referenced => state.remove_busy(),
        _ => {}
    }
}

pub(super) fn close(id: u64) {
    set_ref(id, false);
    let state = state();
    state.callbacks.borrow_mut().remove(&id);
    let Some(resource) = state.resources.borrow_mut().remove(&id) else { return };
    let Ok(entry) = resource.downcast::<Entry>() else { return };
    let Entry { task, kind, .. } = *entry;
    if let Some(task) = task {
        task.abort();
    }
    match kind {
        Kind::Stream { reader, writer, .. } => {
            if let Some(reader) = reader {
                reader.abort();
            }
            state.shared.tokio.spawn(async move { drop(writer.lock().await.take()) });
        }
        Kind::Listener { path: Some(path), .. } if cfg!(unix) => {
            let _ = std::fs::remove_file(path);
        }
        Kind::Listener { .. } => {}
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectOptions {
    #[serde(default)]
    addresses: Vec<(String, u16)>,
    path: Option<String>,
    local_address: Option<String>,
    local_port: Option<u16>,
    attempt_timeout: Option<u64>,
}

pub(super) fn parse_options<'js, T: serde::de::DeserializeOwned>(
    ctx: &Ctx<'js>,
    options: Value<'js>,
) -> rquickjs::Result<T> {
    let json = to_json(ctx, options).map_err(|error| throw(ctx, &error))?;
    serde_json::from_value(json).map_err(|error| throw(ctx, &JsError::type_error(error.to_string())))
}

pub(super) fn parse_ip(ctx: &Ctx<'_>, address: &str, port: u16) -> rquickjs::Result<SocketAddr> {
    let ip = address.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>();
    let ip = ip.map_err(|_| throw(ctx, &JsError::type_error(format!("Invalid IP address: {address}"))))?;
    Ok(SocketAddr::new(ip, port))
}

async fn connect_tcp(
    targets: Vec<SocketAddr>,
    local: Option<SocketAddr>,
    attempt: Option<u64>,
) -> Result<Conn, JsError> {
    let mut last = None;
    let count = targets.len();
    for target in targets {
        let result = async {
            let socket =
                if target.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
            if let Some(local) = local {
                socket.bind(local)?;
            }
            match attempt.filter(|_| count > 1) {
                Some(ms) => tokio::time::timeout(Duration::from_millis(ms), socket.connect(target))
                    .await
                    .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into())),
                None => socket.connect(target).await,
            }
        }
        .await;
        match result.and_then(tcp_conn) {
            Ok(conn) => return Ok(conn),
            Err(error) => {
                last = Some(net_error(&error, "connect", Some(&target.ip().to_string()), Some(target.port())))
            }
        }
    }
    Err(last.unwrap_or_else(|| JsError::new("connect EINVAL").with_code("EINVAL")))
}

#[cfg(unix)]
async fn connect_pipe(path: String) -> Result<Conn, JsError> {
    let stream = tokio::net::UnixStream::connect(&path).await;
    let stream = stream.map_err(|error| net_error(&error, "connect", Some(&path), None))?;
    Ok(pipe_conn(Box::new(stream), &path))
}

#[cfg(windows)]
async fn connect_pipe(path: String) -> Result<Conn, JsError> {
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&path) {
            Ok(client) => return Ok(pipe_conn(Box::new(client), &path)),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(net_error(&error, "connect", Some(&path), None)),
        }
    }
}

fn connect<'js>(ctx: Ctx<'js>, options: Value<'js>, tls: Value<'js>, callback: Function<'js>) -> rquickjs::Result<u64> {
    let options: ConnectOptions = parse_options(&ctx, options)?;
    let tls = Tls::from_js(&ctx, tls, false)?;
    let mut targets = Vec::new();
    for (address, port) in &options.addresses {
        targets.push(parse_ip(&ctx, address, *port)?);
    }
    let local = match (&options.local_address, options.local_port) {
        (None, None) => None,
        (address, port) => {
            let fallback = if targets.first().is_some_and(SocketAddr::is_ipv6) { "::" } else { "0.0.0.0" };
            Some(parse_ip(&ctx, address.as_deref().unwrap_or(fallback), port.unwrap_or(0))?)
        }
    };
    let id = insert(&ctx, stream_kind(None).0, Some(callback));
    let state = state();
    let sender = state.sender.clone();
    let (attempt, path) = (options.attempt_timeout, options.path);
    let task = state.shared.tokio.spawn(async move {
        let conn = match path {
            Some(path) => connect_pipe(path).await,
            None => connect_tcp(targets, local, attempt).await,
        };
        match secure(conn, tls).await {
            Ok(conn) => post(&sender, id, Event::Connect(conn)),
            Err(error) => post(&sender, id, Event::Error(error)),
        }
    });
    let _ = state.with_resource(id, |entry: &mut Entry| entry.task = Some(task.abort_handle()));
    Ok(id)
}

fn writer(ctx: &Ctx<'_>, id: u64) -> rquickjs::Result<Writer> {
    let found = state().with_resource(id, |entry: &mut Entry| match &entry.kind {
        Kind::Stream { writer, .. } => Some(writer.clone()),
        Kind::Listener { .. } => None,
    });
    found.ok().flatten().ok_or_else(|| throw(ctx, &JsError::new("This socket is closed").with_code("EPIPE")))
}

fn with_tcp(ctx: &Ctx<'_>, id: u64, f: impl FnOnce(&socket2::Socket) -> std::io::Result<()>) -> rquickjs::Result<()> {
    let result = state().with_resource(id, |entry: &mut Entry| match &entry.kind {
        Kind::Stream { tcp: Some(tcp), .. } => f(tcp),
        _ => Ok(()),
    });
    match result {
        Ok(Err(error)) => Err(throw(ctx, &net_error(&error, "setsockopt", None, None))),
        _ => Ok(()),
    }
}

fn set_paused(id: u64, value: bool) {
    let _ = state().with_resource(id, |entry: &mut Entry| {
        if let Kind::Stream { paused, .. } = &entry.kind {
            paused.send_replace(value);
        }
    });
}

async fn write_all(writer: Writer, bytes: Vec<u8>) -> Result<(), JsError> {
    let mut guard = writer.lock().await;
    let broken = || net_error(&std::io::ErrorKind::BrokenPipe.into(), "write", None, None);
    let stream = guard.as_mut().ok_or_else(broken)?;
    stream.write_all(&bytes).await.map_err(|error| net_error(&error, "write", None, None))?;
    stream.flush().await.map_err(|error| net_error(&error, "write", None, None))
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "connect", connect)?;
    set_fn(&object, "listen", super::net_server::listen)?;
    super::handles::natives(&object)?;
    set_fn(&object, "stopAccepting", |id: u64| {
        let _ = state().with_resource(id, |entry: &mut Entry| entry.task.take().map(|task| task.abort()));
    })?;
    set_fn(&object, "start", |ctx: Ctx<'js>, id: u64, callback: Function<'js>| {
        state().callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
        start(id);
    })?;
    set_fn(&object, "write", |ctx: Ctx<'js>, id: u64, data: TypedArray<'js, u8>| {
        let work = write_all(writer(&ctx, id)?, bytes_of(&data));
        spawn_op(&ctx, work, |ctx, ()| Ok(Value::new_undefined(ctx.clone())))
    })?;
    set_fn(&object, "shutdown", |ctx: Ctx<'js>, id: u64| {
        let writer = writer(&ctx, id)?;
        let work = async move {
            if let Some(stream) = writer.lock().await.as_mut() {
                stream.shutdown().await.map_err(|error| net_error(&error, "shutdown", None, None))?;
            }
            Ok(())
        };
        spawn_op(&ctx, work, |ctx, ()| Ok(Value::new_undefined(ctx.clone())))
    })?;
    set_fn(&object, "close", close)?;
    set_fn(&object, "ref", set_ref)?;
    set_fn(&object, "pause", |id: u64| set_paused(id, true))?;
    set_fn(&object, "resume", |id: u64| set_paused(id, false))?;
    set_fn(&object, "setNoDelay", |ctx: Ctx<'js>, id: u64, enable: bool| {
        with_tcp(&ctx, id, |tcp| tcp.set_tcp_nodelay(enable))
    })?;
    set_fn(&object, "setKeepAlive", |ctx: Ctx<'js>, id: u64, enable: bool, delay: u64| {
        with_tcp(&ctx, id, |tcp| {
            tcp.set_keepalive(enable)?;
            if enable && delay > 0 {
                tcp.set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(Duration::from_secs(delay)))?;
            }
            Ok(())
        })
    })?;
    Ok(object)
}
