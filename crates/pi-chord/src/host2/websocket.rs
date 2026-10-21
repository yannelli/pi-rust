//! Port of upstream `websocket.ts`: a WebSocket server whose connections are accepted on a host as client links,
//! each stamped with the identity its authenticator returns.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use super::connection::{AcceptOptions, LinkOptions, Restamp};
use super::host::Host;
use super::link::{Link, WebSocketLinkOptions, web_socket_link};
use super::wire::PeerKind;
use crate::error::ChordResult;
use crate::json::Json;
use crate::sync::BoxFuture;

/// The identity of the connecting client, from its upgrade request; `None` refuses it.
pub type Authenticator = Arc<dyn Fn(&Request) -> BoxFuture<ChordResult<Option<Json>>> + Send + Sync>;

/// WebSocket peers are always accepted as clients: they carry no roots or code versions and have no host name.
#[derive(Clone)]
pub struct WebSocketAcceptOptions {
    pub principal: Authenticator,
    pub restamp: Option<Restamp>,
    pub max_calls: Option<usize>,
    pub link: LinkOptions,
}

impl WebSocketAcceptOptions {
    pub fn new(principal: Authenticator) -> Self {
        Self { principal, restamp: None, max_calls: None, link: LinkOptions::default() }
    }
}

/// Accepts every connection of `listener` on `host` as a client link with the authenticated identity, until the
/// returned task is aborted. Must be called within a Tokio runtime.
pub fn accept_web_sockets(listener: TcpListener, host: Host, options: WebSocketAcceptOptions) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (host, options) = (host.clone(), options.clone());
            tokio::spawn(async move {
                let (keep, request) = oneshot::channel();
                #[allow(clippy::result_large_err, reason = "the error type is tungstenite's")]
                let callback = move |request: &Request, response: Response| {
                    let _ = keep.send(request.clone());
                    Ok(response)
                };
                let Ok(socket) = accept_hdr_async(stream, callback).await else { return };
                let Ok(request) = request.await else { return };
                // Wrap at once so frames sent while authenticating wait. Large payloads go out deflated; clients may
                // not send packed frames (a small message could inflate to anything).
                let link = web_socket_link(socket, WebSocketLinkOptions { compress: true, accept_packed: false });
                match (options.principal)(&request).await {
                    Ok(Some(principal)) => {
                        let accept = AcceptOptions {
                            principal,
                            kind: PeerKind::Client,
                            name: None,
                            restamp: options.restamp,
                            max_calls: options.max_calls,
                            link: options.link,
                        };
                        if host.accept(link.clone(), accept).is_err() {
                            link.close(Some("refused"));
                        }
                    }
                    Ok(None) => link.close(Some("unauthorized")),
                    Err(_) => link.close(Some("authentication failed")),
                }
            });
        }
    })
}
