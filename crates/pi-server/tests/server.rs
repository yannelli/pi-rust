//! Port of upstream `test/server.test.ts` and `test/listener.test.ts`.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{SERVER_ID, hello_error_code};
use pi_chord::{BoxFuture, ChordError, ChordResult};
use pi_protocol::ServerMessageDecoder;
use pi_server::testing::{TestServerHost, TestServerOptions, create_test_server};
use pi_server::{ByteConnection, ByteConnectionAcceptor, Server, ServerListener, ServerOptions};

#[test]
fn requires_a_canonical_uuid_v4_server_identity() {
    for server_id in ["", "invalid-server"] {
        let error = Server::new(TestServerHost::new(), ServerOptions::new(server_id, vec![])).err().unwrap();
        assert!(error.to_string().contains("serverId"));
    }
}

#[derive(Default)]
struct TimedOutConnection {
    closed: Mutex<bool>,
    final_chunk: Mutex<Option<Vec<u8>>>,
}

impl ByteConnection for TimedOutConnection {
    fn closed(&self) -> bool {
        *self.closed.lock().unwrap()
    }

    fn send(&self, _chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
        Box::pin(std::future::ready(Err(ChordError::error("handshake timeout must use the terminal close frame"))))
    }

    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<ChordResult<()>> {
        *self.final_chunk.lock().unwrap() = final_chunk;
        *self.closed.lock().unwrap() = true;
        Box::pin(std::future::ready(Ok(())))
    }
}

#[tokio::test]
async fn handshake_timeout_closes_with_a_final_hello_error_frame() {
    let mut options = ServerOptions::new(SERVER_ID, vec![]);
    options.max_frame_length = Some(1024);
    options.handshake_timeout = Some(Duration::from_millis(10));
    let server = Server::new(TestServerHost::new(), options).unwrap();
    let connection = Arc::new(TimedOutConnection::default());
    server.accept(connection.clone());

    common::poll_until(|| connection.closed()).await;
    let final_chunk = connection.final_chunk.lock().unwrap().clone().expect("final frame");
    let messages = ServerMessageDecoder::default().push(&final_chunk).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(hello_error_code(&messages[0]), Some("invalid_request"));
    server.close().await.unwrap();
}

#[test]
fn rejects_handshake_timeouts_above_the_maximum_timer_delay() {
    let mut options = ServerOptions::new(SERVER_ID, vec![]);
    options.handshake_timeout = Some(Duration::from_millis(2_147_483_648));
    let error = Server::new(TestServerHost::new(), options).err().unwrap();
    assert!(error.to_string().contains("handshakeTimeoutMs"));
}

struct FailingCloseListener;

#[async_trait]
impl ServerListener for FailingCloseListener {
    async fn start(&self, _accept: ByteConnectionAcceptor) -> ChordResult<()> {
        Ok(())
    }

    async fn close(&self) -> ChordResult<()> {
        Err(ChordError::error("listener close failed"))
    }
}

#[tokio::test]
async fn rejects_close_and_closed_when_listener_shutdown_fails() {
    let options = ServerOptions::new(SERVER_ID, vec![Arc::new(FailingCloseListener)]);
    let server = Server::new(TestServerHost::new(), options).unwrap();
    server.start().await.unwrap();
    assert_eq!(server.close().await.unwrap_err().to_string(), "listener close failed");
    assert_eq!(server.closed().await.unwrap_err().to_string(), "listener close failed");
}

#[derive(Default)]
struct TestListener {
    accept: Mutex<Option<ByteConnectionAcceptor>>,
    close_count: Mutex<usize>,
    start_error: Option<String>,
}

#[async_trait]
impl ServerListener for TestListener {
    async fn start(&self, accept: ByteConnectionAcceptor) -> ChordResult<()> {
        *self.accept.lock().unwrap() = Some(accept);
        match &self.start_error {
            Some(message) => Err(ChordError::error(message.clone())),
            None => Ok(()),
        }
    }

    async fn close(&self) -> ChordResult<()> {
        *self.close_count.lock().unwrap() += 1;
        Ok(())
    }
}

#[tokio::test]
async fn starts_and_closes_every_configured_listener() {
    let (first, second) = (Arc::new(TestListener::default()), Arc::new(TestListener::default()));
    let listeners: Vec<Arc<dyn ServerListener>> = vec![first.clone(), second.clone()];
    let test = create_test_server(TestServerOptions { listeners, ..TestServerOptions::default() }).unwrap();
    test.server.start().await.unwrap();
    assert!(first.accept.lock().unwrap().is_some());
    assert!(second.accept.lock().unwrap().is_some());
    test.server.close().await.unwrap();
    assert_eq!(*first.close_count.lock().unwrap(), 1);
    assert_eq!(*second.close_count.lock().unwrap(), 1);
}

#[tokio::test]
async fn closes_previously_started_listeners_when_startup_fails() {
    let first = Arc::new(TestListener::default());
    let second = Arc::new(TestListener { start_error: Some("listener failed".into()), ..TestListener::default() });
    let listeners: Vec<Arc<dyn ServerListener>> = vec![first.clone(), second.clone()];
    let test = create_test_server(TestServerOptions { listeners, ..TestServerOptions::default() }).unwrap();
    assert_eq!(test.server.start().await.unwrap_err().to_string(), "listener failed");
    assert_eq!(*first.close_count.lock().unwrap(), 1);
    assert_eq!(*second.close_count.lock().unwrap(), 0);
}
