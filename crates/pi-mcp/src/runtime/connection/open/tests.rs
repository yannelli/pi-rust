//! A connection that `close` began never publishes a client, creates a transport, or leaves the
//! `Closed` state again.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::super::{ConnectionOptions, McpServerConnection, ServerState};
use super::Listed;
use crate::client::{McpClient, McpClientOptions};
use crate::config::{McpServerConfig, McpServerEntry};
use crate::error::{McpError, OAuthError};
use crate::jsonrpc::JsonRpcMessage;
use crate::runtime::{InMemoryAuthStorageBackend, McpOAuthCredentialStore};
use crate::transport::memory::in_memory_pair;
use crate::transport::{McpTransport, SharedTransport, TransportEvent};
use crate::types::LATEST_PROTOCOL_VERSION;

fn connection(transports: Arc<AtomicUsize>) -> McpServerConnection {
    let config: McpServerConfig = serde_json::from_value(json!({ "command": "unused" })).unwrap();
    let entry = McpServerEntry::new("fake", config, "test");
    let credentials = McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None);
    let mut options = ConnectionOptions::new(entry, std::env::temp_dir(), credentials);
    options.create_transport = Arc::new(move |_, _, _| {
        transports.fetch_add(1, Ordering::SeqCst);
        Err(McpError::other("no transport in this test"))
    });
    McpServerConnection::new(options)
}

fn listed() -> Listed {
    Listed { tools: Vec::new(), has_resources: false }
}

/// A client connected to an in-memory server that answers `initialize`.
async fn connected_client() -> McpClient {
    let (client_end, server_end) = in_memory_pair();
    let mut events = server_end.start_channel().await.unwrap();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let TransportEvent::Message(JsonRpcMessage::Request { id, method, .. }) = event else { continue };
            if method == "initialize" {
                let info = json!({ "protocolVersion": LATEST_PROTOCOL_VERSION, "capabilities": {},
                    "serverInfo": { "name": "fake", "version": "1" } });
                let _ = server_end.send(&JsonRpcMessage::result(id, info)).await;
            }
        }
    });
    let client = McpClient::new(McpClientOptions::new("pi", "test"));
    client.connect(client_end as SharedTransport).await.unwrap();
    assert!(client.is_connected());
    client
}

#[tokio::test]
async fn publish_refuses_a_client_once_close_began() {
    let connection = connection(Arc::default());
    let client = connected_client().await;
    connection.inner.shutdown.cancel();
    assert!(connection.inner.publish(&client, listed()).is_err());
    let status = connection.inner.lock();
    assert!(status.client.is_none() && status.state != ServerState::Connected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_waiting_for_the_status_lock_sees_a_cancel_made_meanwhile() {
    let connection = connection(Arc::default());
    let client = connected_client().await;
    let guard = connection.inner.lock();
    let inner = connection.inner.clone();
    let publishing = std::thread::spawn(move || inner.publish(&client, listed()).is_err());
    connection.inner.shutdown.cancel();
    drop(guard);
    assert!(publishing.join().unwrap(), "a client was published after the cancel");
    assert!(connection.inner.lock().client.is_none());
}

#[tokio::test]
async fn close_takes_and_closes_a_published_client() {
    let connection = connection(Arc::default());
    let client = connected_client().await;
    connection.inner.publish(&client, listed()).unwrap();
    connection.close().await;
    assert!(!client.is_connected());
    assert!(connection.inner.lock().client.is_none());
    assert_eq!(connection.state(), ServerState::Closed);
}

#[tokio::test]
async fn nothing_after_close_reopens_or_leaves_the_closed_state() {
    let transports = Arc::new(AtomicUsize::new(0));
    let connection = connection(transports.clone());
    connection.close().await;
    assert!(connection.inner.clone().open().await.is_err());
    assert_eq!(transports.load(Ordering::SeqCst), 0);
    connection.inner.mark_needs_auth();
    let _ = connection.inner.connect_failed(McpError::OAuth(OAuthError::AuthorizationRequired));
    let _ = connection.inner.connect_failed(McpError::other("refused"));
    assert_eq!(connection.state(), ServerState::Closed);
}
