//! Natives for `node:http2`: client and server sessions on the h2 crate. A session's bytes come
//! from and go to a JavaScript socket, so `session.socket` is a real `net.Socket` or TLSSocket.

use std::collections::HashMap;
use std::time::Instant;

use bytes::Bytes;
use rquickjs::{Ctx, Function, Object, Persistent, TypedArray, Value};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use super::http2_stream::{self as streams, Headers, Sender, StreamCommand, h2_error};
use super::net::parse_options;
use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_of, bytes_to_js, error_value, from_json, throw};
use crate::runtime::scope::spawn_op;
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

pub(super) enum Event {
    Ready(serde_json::Value),
    Out(Vec<u8>),
    Stream(u64, u32, Headers, bool),
    Open(u64, u32),
    Headers(u64, Headers),
    Response(u64, Headers, bool),
    Data(u64, Vec<u8>),
    Trailers(u64, Headers),
    End(u64),
    Reset(u64, u32),
    StreamError(u64, JsError),
    Closed(Option<(u32, bool, String)>),
}

/// Posts events for one session to the JavaScript thread.
#[derive(Clone)]
pub(super) struct Poster {
    id: u64,
    sender: std::sync::mpsc::Sender<Message>,
}

impl Poster {
    pub(super) fn send(&self, event: Event) {
        let id = self.id;
        let task: Task = Box::new(move |ctx| deliver(ctx, id, event));
        let _ = self.sender.send(Message::Task(task));
    }
}

enum Command {
    Request(u64, Headers, bool),
    Stream(u64, StreamCommand),
    Ping(oneshot::Sender<Result<f64, JsError>>),
    Goaway(u32),
    WindowSize(u32),
    Forget(u64),
}

struct Session {
    input: Option<mpsc::UnboundedSender<Bytes>>,
    commands: mpsc::UnboundedSender<Command>,
    tasks: Vec<tokio::task::AbortHandle>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Settings {
    header_table_size: Option<u32>,
    enable_push: Option<bool>,
    initial_window_size: Option<u32>,
    max_frame_size: Option<u32>,
    max_concurrent_streams: Option<u32>,
    max_header_list_size: Option<u32>,
    enable_connect_protocol: Option<bool>,
}

fn headers_js<'js>(ctx: &Ctx<'js>, headers: Headers) -> rquickjs::Result<Value<'js>> {
    from_json(ctx, &json!(headers.into_iter().flat_map(|(name, value)| [name, value]).collect::<Vec<_>>()))
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let callback = state().callbacks.borrow().get(&id).cloned();
    let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else { return };
    let undefined = || Value::new_undefined(ctx.clone());
    let result = (|| match event {
        Event::Ready(info) => function.call::<_, ()>(("ready", 0, from_json(ctx, &info)?)),
        Event::Out(bytes) => function.call::<_, ()>(("out", 0, bytes_to_js(ctx, bytes)?)),
        Event::Stream(key, stream, headers, end) => {
            function.call::<_, ()>(("stream", key, headers_js(ctx, headers)?, end, stream))
        }
        Event::Open(key, stream) => function.call::<_, ()>(("open", key, stream)),
        Event::Headers(key, headers) => function.call::<_, ()>(("headers", key, headers_js(ctx, headers)?)),
        Event::Response(key, headers, end) => function.call::<_, ()>(("response", key, headers_js(ctx, headers)?, end)),
        Event::Data(key, bytes) => function.call::<_, ()>(("data", key, bytes_to_js(ctx, bytes)?)),
        Event::Trailers(key, headers) => function.call::<_, ()>(("trailers", key, headers_js(ctx, headers)?)),
        Event::End(key) => function.call::<_, ()>(("end", key)),
        Event::Reset(key, code) => function.call::<_, ()>(("reset", key, code)),
        Event::StreamError(key, error) => function.call::<_, ()>(("streamError", key, error_value(ctx, &error)?)),
        Event::Closed(None) => function.call::<_, ()>(("closed", 0, undefined())),
        Event::Closed(Some((code, goaway, message))) => {
            function.call::<_, ()>(("closed", 0, from_json(ctx, &json!([code, goaway, message]))?))
        }
    })();
    if let Err(error) = result {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

fn closed(result: Result<(), h2::Error>) -> Event {
    Event::Closed(result.err().map(|error| {
        let code = error.reason().map_or(0, u32::from);
        (code, error.is_go_away() && error.is_remote(), error.to_string())
    }))
}

async fn ping(ping_pong: std::sync::Arc<tokio::sync::Mutex<Option<h2::PingPong>>>) -> Result<f64, JsError> {
    let start = Instant::now();
    let mut guard = ping_pong.lock().await;
    let pinger = guard.as_mut().ok_or_else(|| JsError::new("Session closed").with_code("ERR_HTTP2_PING_CANCEL"))?;
    pinger.ping(h2::Ping::opaque()).await.map_err(|error| h2_error(&error))?;
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

type Streams = HashMap<u64, mpsc::UnboundedSender<StreamCommand>>;
type PingPong = std::sync::Arc<tokio::sync::Mutex<Option<h2::PingPong>>>;

fn open_stream(streams: &mut Streams, key: u64, sender: Sender, post: &Poster) {
    let (tx, rx) = mpsc::unbounded_channel();
    streams.insert(key, tx);
    tokio::spawn(streams::sender_task(key, sender, rx, post.clone()));
}

fn route(streams: &mut Streams, key: u64, command: StreamCommand) {
    if let Some(stream) = streams.get(&key) {
        let _ = stream.send(command);
    } else if let StreamCommand::Data(_, _, done) = command {
        let _ = done.send(Err(JsError::new("The stream has been destroyed").with_code("ERR_STREAM_DESTROYED")));
    }
}

async fn run_client(
    io: tokio::io::DuplexStream,
    settings: Settings,
    mut commands: mpsc::UnboundedReceiver<Command>,
    post: Poster,
) {
    let mut builder = h2::client::Builder::new();
    if let Some(size) = settings.header_table_size {
        builder.header_table_size(size);
    }
    if let Some(push) = settings.enable_push {
        builder.enable_push(push);
    }
    if let Some(size) = settings.initial_window_size {
        builder.initial_window_size(size);
    }
    if let Some(size) = settings.max_frame_size {
        builder.max_frame_size(size);
    }
    if let Some(max) = settings.max_concurrent_streams {
        builder.max_concurrent_streams(max);
    }
    if let Some(max) = settings.max_header_list_size {
        builder.max_header_list_size(max);
    }
    let (mut client, mut connection) = match builder.handshake::<_, Bytes>(io).await {
        Ok(pair) => pair,
        Err(error) => return post.send(closed(Err(error))),
    };
    let ping_pong: PingPong = std::sync::Arc::new(tokio::sync::Mutex::new(connection.ping_pong()));
    let (window_tx, mut window_rx) = mpsc::unbounded_channel::<u32>();
    let driver = post.clone();
    tokio::spawn(async move {
        let result = loop {
            tokio::select! {
                result = &mut connection => break result,
                Some(size) = window_rx.recv() => {
                    let _ = connection.set_initial_window_size(size);
                }
            }
        };
        driver.send(closed(result));
    });
    post.send(Event::Ready(json!({ "maxConcurrentStreams": client.current_max_send_streams() })));
    let mut streams = Streams::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::Request(key, headers, end) => {
                let request = match streams::request(&headers) {
                    Ok(request) => request,
                    Err(error) => {
                        post.send(Event::StreamError(key, error));
                        continue;
                    }
                };
                let sent = match client.clone().ready().await {
                    Ok(ready) => {
                        client = ready;
                        client.send_request(request, end)
                    }
                    Err(error) => Err(error),
                };
                match sent {
                    Ok((response, stream)) => {
                        post.send(Event::Open(key, stream.stream_id().as_u32()));
                        open_stream(&mut streams, key, if end { Sender::Done } else { Sender::Send(stream) }, &post);
                        tokio::spawn(streams::client_response(key, response, post.clone()));
                    }
                    Err(error) => post.send(streams::stream_failure(key, &error)),
                }
            }
            Command::Stream(key, command) => route(&mut streams, key, command),
            Command::Ping(done) => {
                let ping_pong = ping_pong.clone();
                tokio::spawn(async move { done.send(ping(ping_pong).await) });
            }
            Command::Goaway(_) => {}
            Command::Forget(key) => {
                streams.remove(&key);
            }
            Command::WindowSize(size) => {
                let _ = window_tx.send(size);
            }
        }
    }
}

async fn run_server(
    io: tokio::io::DuplexStream,
    settings: Settings,
    mut commands: mpsc::UnboundedReceiver<Command>,
    post: Poster,
) {
    let mut builder = h2::server::Builder::new();
    if let Some(size) = settings.header_table_size {
        builder.header_table_size(size);
    }
    if let Some(size) = settings.initial_window_size {
        builder.initial_window_size(size);
    }
    if let Some(size) = settings.max_frame_size {
        builder.max_frame_size(size);
    }
    if let Some(max) = settings.max_concurrent_streams {
        builder.max_concurrent_streams(max);
    }
    if let Some(max) = settings.max_header_list_size {
        builder.max_header_list_size(max);
    }
    if settings.enable_connect_protocol == Some(true) {
        builder.enable_connect_protocol();
    }
    let mut connection = match builder.handshake::<_, Bytes>(io).await {
        Ok(connection) => connection,
        Err(error) => return post.send(closed(Err(error))),
    };
    let ping_pong: PingPong = std::sync::Arc::new(tokio::sync::Mutex::new(connection.ping_pong()));
    post.send(Event::Ready(json!({ "maxConcurrentStreams": connection.max_concurrent_send_streams() })));
    let mut streams = Streams::new();
    let mut next = 0u64;
    let result = loop {
        tokio::select! {
            accepted = connection.accept() => match accepted {
                Some(Ok((request, respond))) => {
                    next += 1;
                    let id = respond.stream_id().as_u32();
                    let headers = streams::request_headers(&request);
                    let body = request.into_body();
                    post.send(Event::Stream(next, id, headers, body.is_end_stream()));
                    open_stream(&mut streams, next, Sender::Respond(respond), &post);
                    let (key, post) = (next, post.clone());
                    tokio::spawn(async move { streams::receive(key, body, &post).await });
                }
                Some(Err(error)) => break Err(error),
                None => break Ok(()),
            },
            command = commands.recv() => match command {
                Some(Command::Stream(key, command)) => route(&mut streams, key, command),
                Some(Command::Ping(done)) => {
                    let ping_pong = ping_pong.clone();
                    tokio::spawn(async move { done.send(ping(ping_pong).await) });
                }
                Some(Command::Goaway(0)) => connection.graceful_shutdown(),
                Some(Command::Goaway(code)) => connection.abrupt_shutdown(h2::Reason::from(code)),
                Some(Command::WindowSize(size)) => {
                    let _ = connection.set_initial_window_size(size);
                }
                Some(Command::Forget(key)) => {
                    streams.remove(&key);
                }
                Some(Command::Request(key, ..)) => post.send(Event::StreamError(key, JsError::new("Servers cannot send requests"))),
                None => break Ok(()),
            },
        }
    };
    post.send(closed(result));
}

/// Start a session: `callback(event, key, ...)` receives its events, including `out` bytes.
fn session<'js>(ctx: Ctx<'js>, server: bool, settings: Value<'js>, callback: Function<'js>) -> rquickjs::Result<u64> {
    let settings: Settings = parse_options(&ctx, settings)?;
    let state = state();
    let id = state.next_id();
    state.callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
    let post = Poster { id, sender: state.sender.clone() };
    let (h2_io, js_io) = tokio::io::duplex(1 << 16);
    let (mut reader, mut writer) = tokio::io::split(js_io);
    let (input, mut input_rx) = mpsc::unbounded_channel::<Bytes>();
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let tokio = &state.shared.tokio;
    let feed = tokio.spawn(async move {
        while let Some(bytes) = input_rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let _ = writer.shutdown().await;
    });
    let out = post.clone();
    let drain = tokio.spawn(async move {
        let mut buffer = vec![0; 1 << 16];
        while let Ok(count @ 1..) = reader.read(&mut buffer).await {
            out.send(Event::Out(buffer[..count].to_vec()));
        }
    });
    let run = if server {
        tokio.spawn(run_server(h2_io, settings, commands_rx, post))
    } else {
        tokio.spawn(run_client(h2_io, settings, commands_rx, post))
    };
    let tasks = vec![feed.abort_handle(), drain.abort_handle(), run.abort_handle()];
    state.resources.borrow_mut().insert(id, Box::new(Session { input: Some(input), commands, tasks }));
    Ok(id)
}

fn with_session<R>(ctx: &Ctx<'_>, id: u64, f: impl FnOnce(&mut Session) -> R) -> rquickjs::Result<R> {
    state()
        .with_resource(id, f)
        .map_err(|_| throw(ctx, &JsError::new("The session has been destroyed").with_code("ERR_HTTP2_INVALID_SESSION")))
}

fn command(ctx: &Ctx<'_>, id: u64, command: Command) -> rquickjs::Result<()> {
    with_session(ctx, id, |session| {
        let _ = session.commands.send(command);
    })
}

fn headers_arg(list: Vec<String>) -> Headers {
    list.chunks(2).filter(|pair| pair.len() == 2).map(|pair| (pair[0].clone(), pair[1].clone())).collect()
}

fn close(id: u64) {
    let state = state();
    state.callbacks.borrow_mut().remove(&id);
    let Some(resource) = state.resources.borrow_mut().remove(&id) else { return };
    if let Ok(session) = resource.downcast::<Session>() {
        session.tasks.iter().for_each(tokio::task::AbortHandle::abort);
    }
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "session", session)?;
    set_fn(&object, "feed", |ctx: Ctx<'js>, id: u64, data: TypedArray<'js, u8>| {
        with_session(&ctx, id, |session| session.input.as_ref().map(|input| input.send(Bytes::from(bytes_of(&data)))))
            .map(|_| ())
    })?;
    set_fn(&object, "endInput", |ctx: Ctx<'js>, id: u64| with_session(&ctx, id, |session| session.input = None))?;
    set_fn(&object, "request", |ctx: Ctx<'js>, id: u64, key: u64, headers: Vec<String>, end: bool| {
        command(&ctx, id, Command::Request(key, headers_arg(headers), end))
    })?;
    set_fn(&object, "respond", |ctx: Ctx<'js>, id: u64, key: u64, headers: Vec<String>, end: bool| {
        command(&ctx, id, Command::Stream(key, StreamCommand::Respond(headers_arg(headers), end)))
    })?;
    set_fn(&object, "info", |ctx: Ctx<'js>, id: u64, key: u64, headers: Vec<String>| {
        command(&ctx, id, Command::Stream(key, StreamCommand::Informational(headers_arg(headers))))
    })?;
    set_fn(&object, "trailers", |ctx: Ctx<'js>, id: u64, key: u64, headers: Vec<String>| {
        command(&ctx, id, Command::Stream(key, StreamCommand::Trailers(headers_arg(headers))))
    })?;
    set_fn(&object, "reset", |ctx: Ctx<'js>, id: u64, key: u64, code: u32| {
        command(&ctx, id, Command::Stream(key, StreamCommand::Reset(code)))
    })?;
    set_fn(&object, "write", |ctx: Ctx<'js>, id: u64, key: u64, data: TypedArray<'js, u8>, end: bool| {
        let (done, wait) = oneshot::channel();
        command(&ctx, id, Command::Stream(key, StreamCommand::Data(Bytes::from(bytes_of(&data)), end, done)))?;
        let work = async move {
            wait.await.unwrap_or_else(|_| {
                Err(JsError::new("The session has been destroyed").with_code("ERR_HTTP2_INVALID_SESSION"))
            })
        };
        spawn_op(&ctx, work, |ctx, ()| Ok(Value::new_undefined(ctx.clone())))
    })?;
    set_fn(&object, "ping", |ctx: Ctx<'js>, id: u64| {
        let (done, wait) = oneshot::channel();
        command(&ctx, id, Command::Ping(done))?;
        let work = async move {
            wait.await.unwrap_or_else(|_| Err(JsError::new("Ping canceled").with_code("ERR_HTTP2_PING_CANCEL")))
        };
        spawn_op(&ctx, work, |ctx, ms| Ok(Value::new_number(ctx.clone(), ms)))
    })?;
    set_fn(&object, "goaway", |ctx: Ctx<'js>, id: u64, code: u32| command(&ctx, id, Command::Goaway(code)))?;
    set_fn(&object, "windowSize", |ctx: Ctx<'js>, id: u64, size: u32| command(&ctx, id, Command::WindowSize(size)))?;
    set_fn(&object, "forget", |ctx: Ctx<'js>, id: u64, key: u64| command(&ctx, id, Command::Forget(key)))?;
    set_fn(&object, "close", close)?;
    Ok(object)
}
