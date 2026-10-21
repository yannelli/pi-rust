//! WebSocket links (upstream `WebSocketLink`): a frame is a text message, or a binary one when its payload is
//! packed. Frames sent before the socket opens wait in order. Sizes are reported, so flow control applies.

use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, connect_async};

use super::pack::{Message, pack_message, parse_frame, unpack_message};
use super::{BaseLink, Link, Transport};
use crate::host2::wire::Frame;

/// How long a closing link waits for the peer's close reply before dropping the socket.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Options of a WebSocket link.
#[derive(Clone, Copy, Debug)]
pub struct WebSocketLinkOptions {
    /// Deflate large payloads of outgoing frames (default false; the server's side of client sockets).
    pub compress: bool,
    /// Accept packed (binary) frames from the peer (default true; false for a server's untrusted clients).
    pub accept_packed: bool,
}

impl Default for WebSocketLinkOptions {
    fn default() -> Self {
        Self { compress: false, accept_packed: true }
    }
}

enum Outgoing {
    Message(WsMessage),
    Close,
}

/// The WebSocket side of [`BaseLink`]: an outbox the socket task writes in order.
pub struct WebSocketTransport {
    outbox: mpsc::UnboundedSender<Outgoing>,
    compress: bool,
}

pub type WebSocketLink = BaseLink<WebSocketTransport>;

impl Transport for WebSocketTransport {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        let message = pack_message(frame, self.compress);
        let size = message.size();
        let message = match message {
            Message::Text(text) => WsMessage::Text(text.into()),
            Message::Binary(bytes) => WsMessage::Binary(bytes.into()),
        };
        let _ = self.outbox.send(Outgoing::Message(message));
        Some(size)
    }

    fn shutdown(&self) {
        let _ = self.outbox.send(Outgoing::Close);
    }
}

/// A link on an open WebSocket. Must be called within a Tokio runtime.
pub fn web_socket_link<S>(socket: WebSocketStream<S>, options: WebSocketLinkOptions) -> Arc<WebSocketLink>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    spawn_link(async move { Some(socket) }, options)
}

/// A link to the WebSocket server at `url` (`ws://` or `wss://`), usable at once: frames wait until the socket
/// opens. A failed connection closes the link (`socket closed`). Must be called within a Tokio runtime.
pub fn connect_web_socket(url: &str, options: WebSocketLinkOptions) -> Arc<WebSocketLink> {
    let url = url.to_string();
    spawn_link(async move { connect_async(url).await.ok().map(|(socket, _)| socket) }, options)
}

fn spawn_link<S, F>(socket: F, options: WebSocketLinkOptions) -> Arc<WebSocketLink>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Future<Output = Option<WebSocketStream<S>>> + Send + 'static,
{
    let (outbox, queue) = mpsc::unbounded_channel();
    let link = Arc::new(BaseLink::new(WebSocketTransport { outbox, compress: options.compress }));
    let weak = Arc::downgrade(&link);
    tokio::spawn(async move {
        if let Some(socket) = socket.await {
            pump(socket, queue, &weak, options.accept_packed).await;
        }
        if let Some(link) = weak.upgrade() {
            link.ended("socket closed");
        }
    });
    link
}

/// Writes the outbox and delivers what arrives, until either side closes.
async fn pump<S>(
    mut socket: WebSocketStream<S>,
    mut queue: mpsc::UnboundedReceiver<Outgoing>,
    link: &Weak<WebSocketLink>,
    accept_packed: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = tokio::time::sleep(Duration::MAX);
    tokio::pin!(deadline);
    let mut closing = false;
    loop {
        tokio::select! {
            outgoing = queue.recv(), if !closing => match outgoing {
                Some(Outgoing::Message(message)) => {
                    if socket.send(message).await.is_err() {
                        return;
                    }
                }
                Some(Outgoing::Close) | None => {
                    closing = true;
                    let frame = CloseFrame { code: CloseCode::Normal, reason: "".into() };
                    if socket.close(Some(frame)).await.is_err() {
                        return;
                    }
                    deadline.as_mut().reset(tokio::time::Instant::now() + CLOSE_WAIT);
                }
            },
            incoming = socket.next() => {
                let Some(Ok(message)) = incoming else { return };
                let Some(link) = link.upgrade() else { return };
                let received = match message {
                    WsMessage::Text(text) => parse_frame(text.as_str()),
                    WsMessage::Binary(bytes) if accept_packed => unpack_message(&bytes),
                    WsMessage::Binary(_) => {
                        Link::close(link.as_ref(), Some("protocol error"));
                        continue;
                    }
                    _ => continue,
                };
                match received {
                    Ok(received) => link.deliver(received),
                    Err(_) => Link::close(link.as_ref(), Some("protocol error")),
                }
            }
            () = &mut deadline, if closing => return,
        }
    }
}
