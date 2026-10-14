//! Port of upstream `radius-relay.ts`: the server's multiplexed Radius host connection. The client byte
//! transport and reconnect are in `radius_client.rs`, and the WebSocket parts in `radius_socket.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use pi_chord::{BoxFuture, ChordError, ChordResult};
use pi_server::{ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::radius_auth::RadiusRelayAuthResolver;
pub use super::radius_socket::*;
use super::relay::{RadiusRelayHostStatus, RelayHost, RelayHostFactory, RelayStatusListener};

const HOST_RETRY_INITIAL: Duration = Duration::from_millis(1_000);
const HOST_RETRY_MAX: Duration = Duration::from_millis(30_000);
const MISSING_AUTH_RETRY: Duration = Duration::from_millis(30_000);
// Browser WebSockets let callers close only with 1000 or 3000 through 4999; RFC codes such as 1002 and
// 1011 may be received but not sent.
const LOCAL_PROTOCOL_ERROR_CLOSE_CODE: u16 = 4000;
const LOCAL_TRANSPORT_ERROR_CLOSE_CODE: u16 = 4001;

pub struct RadiusRelayHostOptions {
    pub server_id: String,
    /// The server's `accept`.
    pub accept: ByteConnectionAcceptor,
    pub auth: Arc<RadiusRelayAuthResolver>,
    pub web_socket_factory: Option<RadiusRelayWebSocketFactory>,
    pub on_status: Option<RelayStatusListener>,
}

/// A relay host factory for `StartServerOptions::relay`.
pub fn radius_relay_host_factory(
    auth: Arc<RadiusRelayAuthResolver>,
    web_socket_factory: Option<RadiusRelayWebSocketFactory>,
) -> RelayHostFactory {
    Arc::new(move |options| {
        Arc::new(RadiusRelayHost::new(RadiusRelayHostOptions {
            server_id: options.server_id,
            accept: options.accept,
            auth: auth.clone(),
            web_socket_factory: web_socket_factory.clone(),
            on_status: options.on_status,
        }))
    })
}

type Active = (Arc<RelayServerByteConnection>, ByteConnectionHandler);
type OpenSocket = (Arc<dyn RadiusRelayWebSocket>, Arc<OrderedWebSocketWriter>);

struct HostInner {
    options: RadiusRelayHostOptions,
    cancel: CancellationToken,
    connections: Mutex<HashMap<String, Active>>,
    socket: Mutex<Option<OpenSocket>>,
    closed: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Maintain the experimental server's multiplexed, authenticated Radius host connection.
pub struct RadiusRelayHost {
    inner: Arc<HostInner>,
    run: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl RadiusRelayHost {
    pub fn new(options: RadiusRelayHostOptions) -> Self {
        let inner = HostInner {
            options,
            cancel: CancellationToken::new(),
            connections: Mutex::default(),
            socket: Mutex::default(),
            closed: AtomicBool::new(false),
        };
        Self { inner: Arc::new(inner), run: Mutex::default() }
    }
}

impl RelayHost for RadiusRelayHost {
    fn start(&self) {
        let mut run = lock(&self.run);
        if run.is_none() {
            *run = Some(tokio::spawn(self.inner.clone().run()));
        }
    }

    fn close(&self) -> BoxFuture<ChordResult<()>> {
        let (inner, run) = (self.inner.clone(), lock(&self.run).take());
        Box::pin(async move {
            if inner.closed.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            inner.cancel.cancel();
            if let Some((socket, writer)) = lock(&inner.socket).take() {
                writer.close();
                close_web_socket(socket.as_ref(), 1000, "Pi server stopped");
            }
            inner.drop_connections(None);
            if let Some(run) = run {
                let _ = run.await;
            }
            Ok(())
        })
    }
}

/// Sleep for `duration`; false when `cancel` fired first.
async fn delay(duration: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = tokio::time::sleep(duration) => true,
        () = cancel.cancelled() => false,
    }
}

fn control(kind: &str, id: &str, code: u16) -> Value {
    json!({ "version": 1, "type": kind, "connection_id": id, "code": code })
}

impl HostInner {
    fn status(&self, status: RadiusRelayHostStatus) {
        if let Some(listener) = &self.options.on_status {
            listener(&status);
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.cancel.is_cancelled()
    }

    async fn run(self: Arc<Self>) {
        let mut retry = HOST_RETRY_INITIAL;
        while !self.is_closed() {
            let result: ChordResult<()> = async {
                let Some(auth) = self.options.auth.resolve(false, Some(&self.cancel)).await? else {
                    self.status(RadiusRelayHostStatus::NotAuthenticated);
                    delay(MISSING_AUTH_RETRY, &self.cancel).await;
                    return Ok(());
                };
                self.status(RadiusRelayHostStatus::Connecting);
                let url = relay_web_socket_url(&auth.gateway, &self.options.server_id)?;
                let factory = self.options.web_socket_factory.clone().unwrap_or_else(default_web_socket_factory);
                let protocol = RADIUS_RELAY_HOST_SUBPROTOCOL;
                let socket = open_radius_relay_web_socket(url, protocol, &auth.token, &factory, Some(&self.cancel));
                let socket = socket.await?;
                retry = HOST_RETRY_INITIAL;
                self.status(RadiusRelayHostStatus::Connected);
                self.serve(socket).await?;
                match self.is_closed() {
                    true => Ok(()),
                    false => Err(ChordError::error("Radius relay host disconnected")),
                }
            }
            .await;
            let Err(error) = result else { continue };
            if self.is_closed() {
                break;
            }
            self.status(RadiusRelayHostStatus::Retrying { error: error.to_string() });
            if !delay(retry, &self.cancel).await {
                break;
            }
            retry = (retry * 2).min(HOST_RETRY_MAX);
        }
    }

    async fn serve(self: &Arc<Self>, (socket, mut events): RelaySocket) -> ChordResult<()> {
        let writer = Arc::new(OrderedWebSocketWriter::new(socket.clone()));
        *lock(&self.socket) = Some((socket.clone(), writer.clone()));
        let result = loop {
            let event = tokio::select! {
                event = events.recv() => event,
                () = self.cancel.cancelled() => break Ok(()),
            };
            match event {
                Some(RelaySocketEvent::Message(frame)) => {
                    if let Err(error) = self.handle_host_message(frame) {
                        let reason = "Radius relay protocol error";
                        close_web_socket(socket.as_ref(), LOCAL_PROTOCOL_ERROR_CLOSE_CODE, reason);
                        break Err(error);
                    }
                }
                Some(RelaySocketEvent::Close { code: 1000, .. }) => break Ok(()),
                Some(RelaySocketEvent::Close { code, reason }) => {
                    let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
                    break Err(ChordError::error(format!("Radius relay host closed ({code}{reason})")));
                }
                Some(RelaySocketEvent::Error { message }) => break Err(web_socket_error(message.as_deref())),
                Some(RelaySocketEvent::Open { .. }) => {}
                None => break Err(web_socket_error(None)),
            }
        };
        writer.close();
        let mut current = lock(&self.socket);
        if current.as_ref().is_some_and(|(open, _)| Arc::ptr_eq(open, &socket)) {
            *current = None;
        }
        drop(current);
        self.drop_connections(result.as_ref().err());
        result
    }

    fn handle_host_message(self: &Arc<Self>, frame: RelayFrame) -> ChordResult<()> {
        let bytes = match frame {
            RelayFrame::Text(text) => {
                let message = parse_host_control_message(&text)?;
                let id = message["connection_id"].as_str().unwrap_or_default().to_string();
                match message["type"].as_str() {
                    Some("ping") => {
                        let host = self.clone();
                        tokio::spawn(async move {
                            if host.send_control(json!({ "version": 1, "type": "pong" })).await.is_err()
                                && let Some((socket, _)) = lock(&host.socket).clone()
                            {
                                let reason = "Radius relay send failed";
                                close_web_socket(socket.as_ref(), LOCAL_TRANSPORT_ERROR_CLOSE_CODE, reason);
                            }
                        });
                    }
                    Some("connection_open") => self.open_connection(id)?,
                    Some("connection_close") => self.remote_close_connection(&id),
                    _ => {}
                }
                return Ok(());
            }
            RelayFrame::Binary(bytes) => bytes,
        };
        let parsed = parse_relay_data_frame(&bytes);
        let (id, payload) = parsed.ok_or_else(|| ChordError::error("Invalid Radius relay data frame"))?;
        let handler = lock(&self.connections).get(&id).map(|(_, handler)| handler.clone());
        match handler {
            Some(handler) => handler.on_data(&payload),
            None => self.spawn_control(control("connection_close", &id, 1000)),
        }
        Ok(())
    }

    fn spawn_control(self: &Arc<Self>, message: Value) {
        let host = self.clone();
        tokio::spawn(async move { host.send_control(message).await });
    }

    fn open_connection(self: &Arc<Self>, id: String) -> ChordResult<()> {
        if lock(&self.connections).contains_key(&id) {
            return Err(ChordError::error("Radius relay reused a connection ID"));
        }
        let host = Arc::downgrade(self);
        let connection = Arc::new(RelayServerByteConnection { host, id: id.clone(), closed: AtomicBool::new(false) });
        let handler = (self.options.accept)(connection.clone());
        if connection.closed() {
            self.spawn_control(control("connection_close", &id, 1012));
        } else {
            lock(&self.connections).insert(id, (connection, handler));
        }
        Ok(())
    }

    fn remote_close_connection(&self, id: &str) {
        let Some((connection, handler)) = lock(&self.connections).remove(id) else { return };
        connection.closed.store(true, Ordering::SeqCst);
        handler.on_close();
    }

    async fn server_close_connection(self: Arc<Self>, id: String, final_chunk: Option<Vec<u8>>) -> ChordResult<()> {
        if lock(&self.connections).remove(&id).is_none() {
            return Ok(());
        }
        if let Some(chunk) = final_chunk {
            self.writer()?.send(RelayFrame::Binary(encode_relay_data_frame(&id, &chunk)?)).await?;
        }
        self.send_control(control("connection_close", &id, 1000)).await
    }

    fn send_data(&self, id: &str, chunk: &[u8]) -> BoxFuture<ChordResult<()>> {
        if !lock(&self.connections).contains_key(id) {
            return Box::pin(async { Err(ChordError::error("Radius relay connection is closed")) });
        }
        let frame = encode_relay_data_frame(id, chunk);
        match self.writer().and_then(|writer| Ok(writer.send(RelayFrame::Binary(frame?)))) {
            Ok(sent) => sent,
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn send_control(&self, message: Value) -> BoxFuture<ChordResult<()>> {
        match self.writer() {
            Ok(writer) => writer.send(RelayFrame::Text(message.to_string())),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn writer(&self) -> ChordResult<Arc<OrderedWebSocketWriter>> {
        let socket = lock(&self.socket);
        let writer = socket.as_ref().map(|(_, writer)| writer.clone());
        writer.ok_or_else(|| ChordError::error("Radius relay host is disconnected"))
    }

    fn drop_connections(&self, error: Option<&ChordError>) {
        let connections: Vec<Active> = lock(&self.connections).drain().map(|(_, active)| active).collect();
        for (connection, handler) in connections {
            connection.closed.store(true, Ordering::SeqCst);
            match error {
                Some(error) => handler.on_error(ChordError::error(error.to_string())),
                None => handler.on_close(),
            }
        }
    }
}

/// One relayed connection as the server sees it.
struct RelayServerByteConnection {
    host: Weak<HostInner>,
    id: String,
    closed: AtomicBool,
}

impl ByteConnection for RelayServerByteConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
        match self.host.upgrade().filter(|_| !self.closed()) {
            Some(host) => host.send_data(&self.id, &chunk),
            None => Box::pin(async { Err(ChordError::error("Radius relay connection is closed")) }),
        }
    }

    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<ChordResult<()>> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Box::pin(async { Ok(()) });
        }
        match self.host.upgrade() {
            Some(host) => Box::pin(host.server_close_connection(self.id.clone(), final_chunk)),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

/// A validated host control message as JSON.
fn parse_host_control_message(value: &str) -> ChordResult<Value> {
    let invalid = || ChordError::error("Invalid Radius relay control message");
    let message: Value = serde_json::from_str(value).map_err(|_| invalid())?;
    if !message.is_object() {
        return Err(invalid());
    }
    if message["version"] != json!(1) {
        return Err(ChordError::error("Unsupported Radius relay control version"));
    }
    let kind = message["type"].as_str().unwrap_or_default();
    if kind == "ping" || kind == "pong" {
        return Ok(json!({ "version": 1, "type": kind }));
    }
    let id = message["connection_id"].as_str().filter(|id| is_connection_id(id));
    let code = &message["code"];
    let code_ok = code.is_null() || code.as_u64().is_some_and(|code| (1000..=4999).contains(&code));
    match (kind, id) {
        ("connection_open" | "connection_close", Some(id)) if code_ok => {
            let mut parsed = json!({ "version": 1, "type": kind, "connection_id": id });
            if kind == "connection_close" && !code.is_null() {
                parsed["code"] = code.clone();
            }
            Ok(parsed)
        }
        _ => Err(invalid()),
    }
}
