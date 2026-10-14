//! The WebSocket side of upstream `radius-relay.ts`: the socket interface, the ordered writer, the data frame
//! envelope, and the opening handshake. A socket reports its events on a channel instead of listeners.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use pi_chord::{ChordError, ChordResult};
use regex::Regex;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Message, http::HeaderValue};
use tokio_util::sync::CancellationToken;

pub const RADIUS_RELAY_HOST_SUBPROTOCOL: &str = "pi-session-relay.host.v1";
pub const RADIUS_RELAY_CLIENT_SUBPROTOCOL: &str = "pi-session-relay.client.v1";

const RELAY_DATA_HEADER_BYTES: usize = 18;
const RELAY_DATA_FRAME_VERSION: u8 = 1;
const RELAY_DATA_FRAME_TYPE: u8 = 1;
const MAX_PENDING_BYTES: usize = pi_protocol::DEFAULT_MAX_FRAME_LENGTH as usize * 4;
const DRAIN_THRESHOLD_BYTES: usize = 1024 * 1024;

static CONNECTION_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
        .unwrap_or_else(|_| unreachable!("valid pattern"))
});

pub fn is_connection_id(value: &str) -> bool {
    CONNECTION_ID.is_match(value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayFrame {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelaySocketEvent {
    Open { protocol: String },
    Message(RelayFrame),
    Close { code: u16, reason: String },
    Error { message: Option<String> },
}

/// The part of a browser-style WebSocket the relay uses (upstream `RadiusRelayWebSocket`).
pub trait RadiusRelayWebSocket: Send + Sync {
    fn is_open(&self) -> bool;
    fn buffered_amount(&self) -> usize;
    /// Queue one message; fails unless the socket is open.
    fn send(&self, frame: RelayFrame) -> ChordResult<()>;
    /// Fails for a code other than 1000 or 3000 through 4999, as browser WebSockets do.
    fn close(&self, code: u16, reason: &str) -> ChordResult<()>;
}

pub type RelaySocket = (Arc<dyn RadiusRelayWebSocket>, mpsc::UnboundedReceiver<RelaySocketEvent>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadiusRelayWebSocketRequest {
    pub url: String,
    pub protocol: String,
    pub authorization: String,
}

pub type RadiusRelayWebSocketFactory = Arc<dyn Fn(RadiusRelayWebSocketRequest) -> RelaySocket + Send + Sync>;

pub fn encode_relay_data_frame(connection_id: &str, payload: &[u8]) -> ChordResult<Vec<u8>> {
    if !is_connection_id(connection_id) {
        return Err(ChordError::type_error("Invalid Radius relay connection ID"));
    }
    let mut frame = vec![RELAY_DATA_FRAME_VERSION, RELAY_DATA_FRAME_TYPE];
    let hex = connection_id.replace('-', "");
    for index in 0..16 {
        frame.push(u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap_or_default());
    }
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// The connection ID and payload of a data frame, or `None` for an invalid one.
pub fn parse_relay_data_frame(frame: &[u8]) -> Option<(String, Vec<u8>)> {
    if frame.len() < RELAY_DATA_HEADER_BYTES
        || frame[0] != RELAY_DATA_FRAME_VERSION
        || frame[1] != RELAY_DATA_FRAME_TYPE
    {
        return None;
    }
    let hex: String = frame[2..RELAY_DATA_HEADER_BYTES].iter().map(|byte| format!("{byte:02x}")).collect();
    let id = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
    is_connection_id(&id).then(|| (id, frame[RELAY_DATA_HEADER_BYTES..].to_vec()))
}

pub fn relay_web_socket_url(gateway: &str, server_id: &str) -> ChordResult<String> {
    let base = url::Url::parse(gateway).map_err(ChordError::other)?;
    let mut url = base.join(&format!("/v1/session-relays/{server_id}/connect")).map_err(ChordError::other)?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => return Err(ChordError::error(format!("Unsupported Radius gateway protocol: {other}:"))),
    };
    let _ = url.set_scheme(scheme);
    Ok(url.to_string())
}

pub fn close_web_socket(socket: &dyn RadiusRelayWebSocket, code: u16, reason: &str) {
    // A WebSocket can reject close() while its opening handshake is still pending.
    let _ = socket.close(code, reason);
}

pub fn web_socket_error(message: Option<&str>) -> ChordError {
    let message = message.map(str::trim).filter(|message| !message.is_empty());
    ChordError::error(message.unwrap_or("Radius WebSocket connection failed"))
}

type Write = (RelayFrame, usize, oneshot::Sender<ChordResult<()>>);

/// Sends in call order with a bound on pending bytes, and waits while the socket buffers too much.
pub struct OrderedWebSocketWriter {
    queue: mpsc::UnboundedSender<Write>,
    pending: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

impl OrderedWebSocketWriter {
    pub fn new(socket: Arc<dyn RadiusRelayWebSocket>) -> Self {
        let (queue, mut writes) = mpsc::unbounded_channel::<Write>();
        let (pending, closed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let (task_pending, task_closed) = (pending.clone(), closed.clone());
        tokio::spawn(async move {
            while let Some((frame, bytes, done)) = writes.recv().await {
                let result = async {
                    if task_closed.load(Ordering::SeqCst) || !socket.is_open() {
                        return Err(ChordError::error("Radius relay WebSocket is closed"));
                    }
                    socket.send(frame)?;
                    while socket.buffered_amount() > DRAIN_THRESHOLD_BYTES {
                        if task_closed.load(Ordering::SeqCst) || !socket.is_open() {
                            return Err(ChordError::error("Radius relay WebSocket closed during write"));
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Ok(())
                }
                .await;
                task_pending.fetch_sub(bytes, Ordering::SeqCst);
                let _ = done.send(result);
            }
        });
        Self { queue, pending, closed }
    }

    /// Queue `frame` before returning; the future reports the send.
    pub fn send(&self, frame: RelayFrame) -> BoxFuture<'static, ChordResult<()>> {
        if self.closed.load(Ordering::SeqCst) {
            return Box::pin(async { Err(ChordError::error("Radius relay WebSocket is closed")) });
        }
        let bytes = match &frame {
            RelayFrame::Text(text) => text.len(),
            RelayFrame::Binary(bytes) => bytes.len(),
        };
        if self.pending.load(Ordering::SeqCst) + bytes > MAX_PENDING_BYTES {
            return Box::pin(async { Err(ChordError::error("Radius relay exceeded its pending byte limit")) });
        }
        self.pending.fetch_add(bytes, Ordering::SeqCst);
        let (done, sent) = oneshot::channel();
        if self.queue.send((frame, bytes, done)).is_err() {
            self.pending.fetch_sub(bytes, Ordering::SeqCst);
        }
        Box::pin(
            async move { sent.await.unwrap_or_else(|_| Err(ChordError::error("Radius relay WebSocket is closed"))) },
        )
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Open a socket and wait for its handshake with the expected subprotocol.
pub async fn open_radius_relay_web_socket(
    url: String,
    protocol: &str,
    token: &str,
    factory: &RadiusRelayWebSocketFactory,
    signal: Option<&CancellationToken>,
) -> ChordResult<RelaySocket> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(ChordError::error("The operation was aborted"));
    }
    let request =
        RadiusRelayWebSocketRequest { url, protocol: protocol.to_string(), authorization: format!("Bearer {token}") };
    let (socket, mut events) = factory(request);
    let never = CancellationToken::new();
    let signal = signal.unwrap_or(&never);
    let error = loop {
        let event = tokio::select! {
            event = events.recv() => event,
            () = signal.cancelled() => break ChordError::error("Radius relay connection cancelled"),
        };
        match event {
            Some(RelaySocketEvent::Open { protocol: selected }) if selected == protocol => {
                return Ok((socket, events));
            }
            Some(RelaySocketEvent::Open { protocol: selected }) => {
                let selected = serde_json::to_string(&selected).unwrap_or_default();
                break ChordError::error(format!("Radius relay selected unexpected WebSocket protocol {selected}"));
            }
            Some(RelaySocketEvent::Close { code, .. }) => {
                break ChordError::error(format!("Radius relay closed before connecting ({code})"));
            }
            Some(RelaySocketEvent::Error { message }) => break web_socket_error(message.as_deref()),
            Some(RelaySocketEvent::Message(_)) => {}
            None => break web_socket_error(None),
        }
    };
    close_web_socket(socket.as_ref(), 1000, "Radius relay connection failed");
    Err(error)
}

/// A tokio-tungstenite WebSocket behind the browser-style interface (upstream uses undici's WebSocket).
struct TungsteniteSocket {
    open: Arc<AtomicBool>,
    buffered: Arc<AtomicUsize>,
    outgoing: mpsc::UnboundedSender<Message>,
}

impl RadiusRelayWebSocket for TungsteniteSocket {
    fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    fn buffered_amount(&self) -> usize {
        self.buffered.load(Ordering::SeqCst)
    }

    fn send(&self, frame: RelayFrame) -> ChordResult<()> {
        if !self.is_open() {
            return Err(ChordError::error("WebSocket is not open"));
        }
        let message = match frame {
            RelayFrame::Text(text) => Message::text(text),
            RelayFrame::Binary(bytes) => Message::binary(bytes),
        };
        self.buffered.fetch_add(message.len(), Ordering::SeqCst);
        self.outgoing.send(message).map_err(|_| ChordError::error("WebSocket is not open"))
    }

    fn close(&self, code: u16, reason: &str) -> ChordResult<()> {
        if code != 1000 && !(3000..=4999).contains(&code) {
            return Err(ChordError::error("Invalid close code"));
        }
        let frame = CloseFrame { code: code.into(), reason: reason.to_string().into() };
        let _ = self.outgoing.send(Message::Close(Some(frame)));
        Ok(())
    }
}

/// Upstream `defaultWebSocketFactory`.
pub fn default_web_socket_factory() -> RadiusRelayWebSocketFactory {
    Arc::new(|request: RadiusRelayWebSocketRequest| {
        let (events, received) = mpsc::unbounded_channel();
        let (outgoing, mut queued) = mpsc::unbounded_channel::<Message>();
        let (open, buffered) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let socket = Arc::new(TungsteniteSocket { open: open.clone(), buffered: buffered.clone(), outgoing });
        tokio::spawn(async move {
            let mut http = match request.url.as_str().into_client_request() {
                Ok(http) => http,
                Err(error) => return drop(events.send(RelaySocketEvent::Error { message: Some(error.to_string()) })),
            };
            for (name, value) in
                [("authorization", &request.authorization), ("sec-websocket-protocol", &request.protocol)]
            {
                if let Ok(value) = HeaderValue::from_str(value) {
                    http.headers_mut().insert(name, value);
                }
            }
            let (stream, response) = match tokio_tungstenite::connect_async(http).await {
                Ok(connected) => connected,
                Err(error) => return drop(events.send(RelaySocketEvent::Error { message: Some(error.to_string()) })),
            };
            let header = response.headers().get("sec-websocket-protocol").and_then(|value| value.to_str().ok());
            open.store(true, Ordering::SeqCst);
            let _ = events.send(RelaySocketEvent::Open { protocol: header.unwrap_or_default().to_string() });
            let (mut sink, mut source) = stream.split();
            let writer = tokio::spawn(async move {
                while let Some(message) = queued.recv().await {
                    let (size, closing) = (message.len(), matches!(message, Message::Close(_)));
                    let sent = sink.send(message).await;
                    buffered.fetch_sub(size.min(buffered.load(Ordering::SeqCst)), Ordering::SeqCst);
                    if sent.is_err() || closing {
                        break;
                    }
                }
            });
            let event = loop {
                match source.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let _ = events.send(RelaySocketEvent::Message(RelayFrame::Text(text.to_string())));
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let _ = events.send(RelaySocketEvent::Message(RelayFrame::Binary(bytes.to_vec())));
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) =
                            frame.map_or((1005, String::new()), |f| (f.code.into(), f.reason.to_string()));
                        break RelaySocketEvent::Close { code, reason };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        let _ = events.send(RelaySocketEvent::Error { message: Some(error.to_string()) });
                        break RelaySocketEvent::Close { code: 1006, reason: String::new() };
                    }
                    None => break RelaySocketEvent::Close { code: 1006, reason: String::new() },
                }
            };
            open.store(false, Ordering::SeqCst);
            writer.abort();
            let _ = events.send(event);
        });
        (socket as Arc<dyn RadiusRelayWebSocket>, received)
    })
}
