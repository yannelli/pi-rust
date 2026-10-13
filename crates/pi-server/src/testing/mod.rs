//! Deterministic hosts and wire clients for server conformance tests.

mod client;
mod host;

use std::sync::Arc;
use std::time::Duration;

use pi_chord::ChordResult;

pub use client::{ProtocolTestClient, WireChannel, connect_in_memory};
pub use host::{Deferred, Gate, TestHarness, TestServerHost, create_test_server_services};

use crate::listener::ServerListener;
use crate::server::Server;
use crate::types::{ErrorReporter, ServerOptions};

/// The server ID test servers use unless told otherwise.
pub const TEST_SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

#[derive(Default)]
pub struct TestServerOptions {
    pub listeners: Vec<Arc<dyn ServerListener>>,
    pub host: Option<Arc<TestServerHost>>,
    pub server_id: Option<String>,
    pub max_frame_length: Option<u32>,
    pub handshake_timeout: Option<Duration>,
    pub on_error: Option<ErrorReporter>,
}

pub struct TestServer {
    pub server: Server<TestServerHost>,
    pub host: Arc<TestServerHost>,
}

/// Create an unstarted server with deterministic defaults for transport conformance tests.
pub fn create_test_server(options: TestServerOptions) -> ChordResult<TestServer> {
    let host = options.host.unwrap_or_default();
    let server = Server::new(
        host.clone(),
        ServerOptions {
            listeners: options.listeners,
            server_id: options.server_id.unwrap_or_else(|| TEST_SERVER_ID.to_string()),
            max_frame_length: options.max_frame_length,
            handshake_timeout: options.handshake_timeout,
            on_connection_count_changed: None,
            on_error: options.on_error,
        },
    )?;
    Ok(TestServer { server, host })
}

/// Connect a recording test client to a Unix socket path.
#[cfg(unix)]
pub async fn connect_unix_test_client(path: &std::path::Path) -> ChordResult<Arc<ProtocolTestClient>> {
    unix::connect(path).await
}

#[cfg(unix)]
mod unix {
    use std::path::Path;
    use std::sync::{Arc, Mutex, Weak};

    use pi_chord::{BoxFuture, ChordError, ChordResult};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::unix::OwnedWriteHalf;

    use super::{ProtocolTestClient, WireChannel};
    use crate::task::lock;

    struct SocketChannel {
        writer: Arc<tokio::sync::Mutex<Option<OwnedWriteHalf>>>,
        reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
        client: Mutex<Weak<ProtocolTestClient>>,
    }

    async fn write(writer: &tokio::sync::Mutex<Option<OwnedWriteHalf>>, chunk: &[u8]) -> ChordResult<()> {
        let mut writer = writer.lock().await;
        let writer = writer.as_mut().ok_or_else(|| ChordError::error("Wire connection closed"))?;
        writer.write_all(chunk).await.map_err(ChordError::other)
    }

    impl WireChannel for SocketChannel {
        fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
            let writer = self.writer.clone();
            Box::pin(async move { write(&writer, &chunk).await })
        }

        fn send_fragmented(&self, chunk: Vec<u8>, split_at: usize) -> BoxFuture<ChordResult<()>> {
            let writer = self.writer.clone();
            Box::pin(async move {
                write(&writer, &chunk[..split_at]).await?;
                write(&writer, &chunk[split_at..]).await
            })
        }

        fn close(&self) -> BoxFuture<()> {
            let writer = self.writer.clone();
            let reader = lock(&self.reader).take();
            let client = lock(&self.client).upgrade();
            Box::pin(async move {
                writer.lock().await.take();
                if let Some(reader) = reader {
                    reader.abort();
                    let _ = reader.await;
                }
                if let Some(client) = client {
                    client.mark_closed();
                }
            })
        }
    }

    pub(super) async fn connect(path: &Path) -> ChordResult<Arc<ProtocolTestClient>> {
        let stream = tokio::net::UnixStream::connect(path).await.map_err(ChordError::other)?;
        let (mut reader, writer) = stream.into_split();
        let channel = Arc::new(SocketChannel {
            writer: Arc::new(tokio::sync::Mutex::new(Some(writer))),
            reader: Mutex::new(None),
            client: Mutex::new(Weak::new()),
        });
        let client = ProtocolTestClient::new(channel.clone());
        *lock(&channel.client) = Arc::downgrade(&client);
        let receiver = client.clone();
        let task = tokio::spawn(async move {
            let mut buffer = vec![0u8; 64 * 1024];
            while let Ok(count) = reader.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
                receiver.receive(&buffer[..count]);
            }
            receiver.mark_closed();
        });
        *lock(&channel.reader) = Some(task);
        Ok(client)
    }
}
