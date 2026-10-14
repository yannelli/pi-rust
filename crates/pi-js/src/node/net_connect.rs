//! Outgoing connections for `net` and `tls`: TCP over the looked-up addresses as Node's
//! autoSelectFamily tries them, and Unix sockets or Windows named pipes.

use std::net::SocketAddr;
use std::time::Duration;

use rquickjs::{Ctx, Function, Value};
use serde::Deserialize;

use super::net::{
    Conn, Entry, Event, Sender, insert, net_error, parse_ip, parse_options, pipe_conn, post, secure, stream_kind,
    tcp_conn,
};
use super::tls::Tls;
use crate::error::JsError;
use crate::runtime::state::state;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectOptions {
    #[serde(default)]
    addresses: Vec<(String, u16)>,
    path: Option<String>,
    local_address: Option<String>,
    local_port: Option<u16>,
    /// Set when JavaScript chose several addresses for autoSelectFamily.
    attempt_timeout: Option<u64>,
}

/// Why a TCP connect gave up: one error, or no address could be reached with autoSelectFamily.
/// JavaScript then reports an AggregateError of the per-address errors it was sent.
enum Failure {
    Error(JsError),
    Exhausted,
}

fn timed_out() -> std::io::Error {
    #[cfg(unix)]
    return std::io::Error::from_raw_os_error(libc::ETIMEDOUT);
    #[cfg(not(unix))]
    return std::io::ErrorKind::TimedOut.into();
}

/// Connect to one address. The flag is true when the time limit stopped it.
async fn try_one(target: SocketAddr, local: Option<SocketAddr>, limit: Option<u64>) -> (std::io::Result<Conn>, bool) {
    let result = async {
        let socket = if target.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
        if let Some(local) = local {
            socket.bind(local)?;
        }
        match limit {
            Some(ms) => tokio::time::timeout(Duration::from_millis(ms), socket.connect(target)).await.ok(),
            None => Some(socket.connect(target).await),
        }
        .transpose()
    }
    .await;
    match result {
        Ok(Some(stream)) => (tcp_conn(stream), false),
        Ok(None) => (Err(timed_out()), true),
        Err(error) => (Err(error), false),
    }
}

/// Node's internalConnectMultiple: one address at a time, and each address but the last gets
/// the time limit before Node moves on. Every start and failure is reported as it happens.
async fn connect_tcp(
    sender: Sender,
    id: u64,
    targets: Vec<SocketAddr>,
    local: Option<SocketAddr>,
    timeout: Option<u64>,
) -> Result<Conn, Failure> {
    let count = targets.len();
    let multiple = timeout.is_some() && count > 1;
    let mut last = None;
    for (index, target) in targets.into_iter().enumerate() {
        post(&sender, id, Event::Attempt(target));
        let limit = timeout.filter(|_| multiple && index + 1 < count);
        let (result, expired) = try_one(target, local, limit).await;
        match result {
            Ok(conn) => return Ok(conn),
            Err(error) => {
                let error = net_error(&error, "connect", Some(&target.ip().to_string()), Some(target.port()));
                let event = if expired { Event::AttemptTimeout } else { Event::AttemptFailed };
                post(&sender, id, event(target, error.clone()));
                last = Some(error);
            }
        }
    }
    if multiple {
        return Err(Failure::Exhausted);
    }
    Err(Failure::Error(last.unwrap_or_else(|| JsError::new("connect EINVAL").with_code("EINVAL"))))
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

pub(super) fn connect<'js>(
    ctx: Ctx<'js>,
    options: Value<'js>,
    tls: Value<'js>,
    callback: Function<'js>,
) -> rquickjs::Result<u64> {
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
    let (timeout, path) = (options.attempt_timeout, options.path);
    let task = state.shared.tokio.spawn(async move {
        let conn = match path {
            Some(path) => connect_pipe(path).await.map_err(Failure::Error),
            None => connect_tcp(sender.clone(), id, targets, local, timeout).await,
        };
        let event = match conn {
            Ok(conn) => match secure(Ok(conn), tls).await {
                Ok(conn) => Event::Connect(conn),
                Err(error) => Event::Error(error),
            },
            Err(Failure::Error(error)) => Event::Error(error),
            Err(Failure::Exhausted) => Event::Exhausted,
        };
        post(&sender, id, event);
    });
    let _ = state.with_resource(id, |entry: &mut Entry| entry.task = Some(task.abort_handle()));
    Ok(id)
}
