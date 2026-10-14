//! Listening half of `node:net` and `node:tls`: TCP listeners with a backlog, Unix socket
//! listeners, and Windows named pipe servers, each accepting on tokio.

use std::net::SocketAddr;
use std::time::Duration;

use rquickjs::{Ctx, Function, Value};
use serde::Deserialize;
use serde_json::json;

use super::net::{
    Entry, Event, Kind, Sender, address_json, insert, net_error, parse_ip, parse_options, pipe_conn, post,
};
use super::net::{secure, tcp_conn};
use super::tls::Tls;
use crate::runtime::convert::{from_json, throw};
use crate::runtime::state::state;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListenOptions {
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    backlog: Option<i32>,
    #[serde(default)]
    ipv6_only: bool,
}

fn bind_tcp(address: SocketAddr, backlog: i32, ipv6_only: bool) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))?;
    if address.is_ipv6() {
        socket.set_only_v6(ipv6_only)?;
    }
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&address.into())?;
    socket.listen(backlog)?;
    socket.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// Hand an accepted connection to JavaScript, after the TLS handshake when there is one.
fn accepted(sender: &Sender, id: u64, conn: super::net::Conn, tls: Option<Tls>) {
    let Some(tls) = tls else { return post(sender, id, Event::Connection(conn)) };
    let sender = sender.clone();
    tokio::spawn(async move {
        match secure(Ok(conn), Some(tls)).await {
            Ok(conn) => post(&sender, id, Event::Connection(conn)),
            Err(error) => post(&sender, id, Event::ClientError(error)),
        }
    });
}

async fn accept_tcp(sender: Sender, id: u64, listener: tokio::net::TcpListener, tls: Option<Tls>) {
    loop {
        match listener.accept().await.and_then(|(stream, _)| tcp_conn(stream)) {
            Ok(conn) => accepted(&sender, id, conn, tls.clone()),
            Err(error) => {
                post(&sender, id, Event::Error(net_error(&error, "accept", None, None)));
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(unix)]
type PipeListener = tokio::net::UnixListener;

#[cfg(unix)]
fn bind_pipe(path: &str) -> std::io::Result<PipeListener> {
    tokio::net::UnixListener::bind(path)
}

#[cfg(unix)]
async fn accept_pipe(sender: Sender, id: u64, listener: PipeListener, path: String, tls: Option<Tls>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => accepted(&sender, id, pipe_conn(Box::new(stream), &path), tls.clone()),
            Err(error) => {
                post(&sender, id, Event::Error(net_error(&error, "accept", None, None)));
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(windows)]
type PipeListener = tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(windows)]
fn bind_pipe(path: &str) -> std::io::Result<PipeListener> {
    tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).create(path)
}

#[cfg(windows)]
async fn accept_pipe(sender: Sender, id: u64, mut server: PipeListener, path: String, tls: Option<Tls>) {
    use tokio::net::windows::named_pipe::ServerOptions;
    loop {
        if let Err(error) = server.connect().await {
            post(&sender, id, Event::Error(net_error(&error, "accept", None, None)));
            continue;
        }
        let next = match ServerOptions::new().create(&path) {
            Ok(next) => next,
            Err(error) => return post(&sender, id, Event::Error(net_error(&error, "listen", None, None))),
        };
        let connected = std::mem::replace(&mut server, next);
        accepted(&sender, id, pipe_conn(Box::new(connected), &path), tls.clone());
    }
}

/// Bind now (so `EADDRINUSE` throws here), then accept on tokio. Returns `{ id, address }`.
pub(super) fn listen<'js>(
    ctx: Ctx<'js>,
    options: Value<'js>,
    tls: Value<'js>,
    callback: Function<'js>,
) -> rquickjs::Result<Value<'js>> {
    let options: ListenOptions = parse_options(&ctx, options)?;
    let tls = Tls::from_js(&ctx, tls, true)?;
    let state = state();
    let _guard = state.shared.tokio.enter();
    let sender = state.sender.clone();
    if let Some(path) = options.path {
        let listener =
            bind_pipe(&path).map_err(|error| throw(&ctx, &net_error(&error, "listen", Some(&path), None)))?;
        let id = insert(&ctx, Kind::Listener { path: Some(path.clone()), socket: None }, Some(callback));
        let task = state.shared.tokio.spawn(accept_pipe(sender, id, listener, path.clone(), tls));
        let _ = state.with_resource(id, |entry: &mut Entry| entry.task = Some(task.abort_handle()));
        return from_json(&ctx, &json!({ "id": id, "address": path }));
    }
    let port = options.port.unwrap_or(0);
    let hosts = match &options.host {
        Some(host) => vec![host.as_str()],
        None => vec!["::", "0.0.0.0"],
    };
    let backlog = options.backlog.unwrap_or(511);
    let mut bound = Err(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable));
    for host in &hosts {
        bound = bind_tcp(parse_ip(&ctx, host, port)?, backlog, options.ipv6_only);
        if bound.as_ref().map_or_else(|error| error.kind() == std::io::ErrorKind::AddrInUse, |_| true) {
            break;
        }
    }
    let listener = bound.map_err(|error| throw(&ctx, &net_error(&error, "listen", Some(hosts[0]), Some(port))))?;
    let address = address_json(listener.local_addr().ok());
    let socket = socket2::SockRef::from(&listener).try_clone().ok();
    let id = insert(&ctx, Kind::Listener { path: None, socket }, Some(callback));
    let task = state.shared.tokio.spawn(accept_tcp(sender, id, listener, tls));
    let _ = state.with_resource(id, |entry: &mut Entry| entry.task = Some(task.abort_handle()));
    from_json(&ctx, &json!({ "id": id, "address": address }))
}

/// Accept on a TCP listener received from another process. Returns `{ id, address }`.
pub(super) fn adopt<'js>(
    ctx: Ctx<'js>,
    socket: socket2::Socket,
    callback: Function<'js>,
) -> rquickjs::Result<Value<'js>> {
    let state = state();
    let _guard = state.shared.tokio.enter();
    let copy = socket.try_clone().ok();
    let listener = socket
        .set_nonblocking(true)
        .and_then(|()| tokio::net::TcpListener::from_std(socket.into()))
        .map_err(|error| throw(&ctx, &net_error(&error, "listen", None, None)))?;
    let address = address_json(listener.local_addr().ok());
    let id = insert(&ctx, Kind::Listener { path: None, socket: copy }, Some(callback));
    let task = state.shared.tokio.spawn(accept_tcp(state.sender.clone(), id, listener, None));
    let _ = state.with_resource(id, |entry: &mut Entry| entry.task = Some(task.abort_handle()));
    from_json(&ctx, &json!({ "id": id, "address": address }))
}
