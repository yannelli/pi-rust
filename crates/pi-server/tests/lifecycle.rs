//! Port of upstream `test/conformance.test.ts`: call admission, Session lifecycle, and
//! "routed Session acquisition failures".

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{SERVER_ID, assert_error, assert_ok, poll_until, session_call};
use pi_chord::services::{ServiceCall, ServiceUpdatePublisher};
use pi_chord::{BoxFuture, ChordError, ChordResult, Context, Json, json};
use pi_protocol::PROTOCOL_VERSION;
use pi_server::testing::{Deferred, TestServerHost, connect_in_memory, create_test_server_services};
use pi_server::{
    BasicSessionMetadata, RoutedServerServiceHost, RoutedSessionAttachment, RoutedSessionHandle, Server, ServerHost,
    ServerOptions,
};

const VERSION: u64 = PROTOCOL_VERSION as u64;

fn server<H: ServerHost>(host: Arc<H>) -> Server<H> {
    Server::new(host, ServerOptions::new(SERVER_ID, vec![])).unwrap()
}

#[tokio::test]
async fn admits_concurrent_service_calls_to_the_attached_session() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    client.attach(SERVER_ID, "session-1").await.unwrap();
    let harness = host.latest_harness("session-1");
    let gate = harness.gate_next_service_call();
    let caller = client.clone();
    let first = tokio::spawn(async move {
        caller.request_session_service(SERVER_ID, "session-1", session_call("run", vec![json!("first")]), None).await
    });
    gate.entered.wait().await;
    let second =
        client.request_session_service(SERVER_ID, "session-1", session_call("run", vec![json!("second")]), None);
    assert_ok(&second.await.unwrap());
    let expected = vec![session_call("run", vec![json!("first")]), session_call("run", vec![json!("second")])];
    assert_eq!(harness.service_calls(), expected);
    gate.release.resolve(());
    assert_ok(&first.await.unwrap().unwrap());
    server.close().await.unwrap();
}

#[tokio::test]
async fn keeps_attachment_demand_until_an_accepted_call_settles_after_disconnect() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    client.attach(SERVER_ID, "session-1").await.unwrap();
    let harness = host.latest_harness("session-1");
    let gate = harness.gate_next_service_call();
    let caller = client.clone();
    let calling = tokio::spawn(async move {
        caller.request_session_service(SERVER_ID, "session-1", session_call("run", vec![]), None).await
    });
    gate.entered.wait().await;

    client.close().await;
    assert_eq!(harness.attached_clients(), 1);
    gate.release.resolve(());
    let error = calling.await.unwrap().unwrap_err();
    assert!(error.to_string().to_lowercase().contains("closed"));
    poll_until(|| harness.attached_clients() == 0).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn rejects_requests_addressed_to_another_server() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    let response = client.attach("00000000-0000-4000-8000-000000000002", "session-1").await.unwrap();
    assert_error(&response, "wrong_server");
    assert_eq!(host.harness_session_count(), 0);
    server.close().await.unwrap();
}

#[tokio::test]
async fn reports_an_unknown_session_without_creating_a_harness() {
    let host = TestServerHost::new();
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    assert_error(&client.attach(SERVER_ID, "missing").await.unwrap(), "session_not_found");
    assert_eq!(host.harness_session_count(), 0);
    server.close().await.unwrap();
}

#[tokio::test]
async fn invalidates_a_terminated_harness_and_allows_a_later_attach() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    client.attach(SERVER_ID, "session-1").await.unwrap();
    let first = host.latest_harness("session-1");

    first.terminate(ChordError::error("worker crashed"));
    first.wait_terminated().await;
    poll_until(|| first.attached_clients() == 0).await;
    assert_eq!(first.attachment_release_count(), 1);
    assert_ok(&client.attach(SERVER_ID, "session-1").await.unwrap());
    assert_eq!(host.harnesses("session-1").len(), 2);
    server.close().await.unwrap();
}

#[tokio::test]
async fn connection_loss_releases_its_attachment_while_shutdown_closes_the_harness() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    client.attach(SERVER_ID, "session-1").await.unwrap();
    let harness = host.latest_harness("session-1");

    client.close().await;
    poll_until(|| harness.attached_clients() == 0).await;
    assert_eq!(harness.close_count(), 0);
    server.close().await.unwrap();
    assert_eq!(harness.close_count(), 1);
}

struct SlowAttachHandle {
    acquiring: Deferred<()>,
    proceed: Deferred<()>,
    terminated: Deferred<Option<ChordError>>,
    releases: Arc<Mutex<usize>>,
}

struct CountingRelease(Arc<Mutex<usize>>);

#[async_trait]
impl RoutedSessionAttachment for CountingRelease {
    async fn invoke_service(
        &self,
        _: ServiceCall,
        _: ServiceUpdatePublisher,
        _: &Context,
    ) -> ChordResult<Option<Json>> {
        Ok(None)
    }
    async fn release(&self, _: &Context) -> ChordResult<()> {
        *self.0.lock().unwrap() += 1;
        Ok(())
    }
}

#[async_trait]
impl RoutedSessionHandle for SlowAttachHandle {
    async fn attach_client(&self, _: &Context) -> ChordResult<Arc<dyn RoutedSessionAttachment>> {
        self.acquiring.resolve(());
        self.proceed.wait().await;
        Ok(Arc::new(CountingRelease(self.releases.clone())))
    }
    fn terminated(&self) -> Option<BoxFuture<Option<ChordError>>> {
        let terminated = self.terminated.clone();
        Some(Box::pin(async move { terminated.wait().await }))
    }
    async fn close(&self, _: &Context) -> ChordResult<()> {
        Ok(())
    }
}

struct SlowHost(Arc<SlowAttachHandle>);

#[async_trait]
impl ServerHost for SlowHost {
    type Metadata = BasicSessionMetadata;
    fn server_services(&self) -> Arc<dyn RoutedServerServiceHost> {
        create_test_server_services()
    }
    async fn resolve_session(&self, id: &str, _: &Context) -> ChordResult<BasicSessionMetadata> {
        Ok(BasicSessionMetadata { id: id.into() })
    }
    async fn open_session(&self, _: BasicSessionMetadata, _: &Context) -> ChordResult<Arc<dyn RoutedSessionHandle>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn releases_a_lease_acquired_concurrently_with_harness_termination() {
    let releases = Arc::new(Mutex::new(0));
    let handle = Arc::new(SlowAttachHandle {
        acquiring: Deferred::new(),
        proceed: Deferred::new(),
        terminated: Deferred::new(),
        releases: releases.clone(),
    });
    let server = server(Arc::new(SlowHost(handle.clone())));
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    let attaching = client.clone();
    let attach = tokio::spawn(async move { attaching.attach(SERVER_ID, "session-1").await });
    handle.acquiring.wait().await;

    handle.terminated.resolve(Some(ChordError::error("worker crashed")));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    handle.proceed.resolve(());
    assert_error(&attach.await.unwrap().unwrap(), "server_draining");
    assert_eq!(*releases.lock().unwrap(), 1);
    server.close().await.unwrap();
}

#[tokio::test]
async fn shares_a_harness_creation_failure_and_allows_a_later_retry() {
    let host = TestServerHost::new();
    host.seed("session-1");
    host.set_next_open_session_error(ChordError::error("Harness creation failed"));
    let server = server(host.clone());
    let (first, second) = (connect_in_memory(&server), connect_in_memory(&server));
    first.hello(VERSION).await.unwrap();
    second.hello(VERSION).await.unwrap();
    let gate = host.gate_next_open_session();

    let attaching = first.clone();
    let first_attach = tokio::spawn(async move { attaching.attach(SERVER_ID, "session-1").await });
    gate.entered.wait().await;
    let attaching = second.clone();
    let second_attach = tokio::spawn(async move { attaching.attach(SERVER_ID, "session-1").await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    gate.release.resolve(());

    assert_error(&first_attach.await.unwrap().unwrap(), "internal_error");
    assert_error(&second_attach.await.unwrap().unwrap(), "internal_error");
    assert_eq!(host.open_session_count(), 1);
    assert_ok(&first.attach(SERVER_ID, "session-1").await.unwrap());
    assert_eq!(host.open_session_count(), 2);
    assert_eq!(host.harnesses("session-1").len(), 1);
    server.close().await.unwrap();
}

#[tokio::test]
async fn closes_a_harness_acquired_while_shutdown_is_in_progress() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    let gate = host.gate_next_open_session();
    let attaching = client.clone();
    let attach = tokio::spawn(async move { attaching.attach(SERVER_ID, "session-1").await });
    gate.entered.wait().await;
    let closing = server.clone();
    let closing = tokio::spawn(async move { closing.close().await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    gate.release.resolve(());

    closing.await.unwrap().unwrap();
    let error = attach.await.unwrap().unwrap_err();
    assert!(error.to_string().to_lowercase().contains("closed"));
    assert_eq!(host.latest_harness("session-1").close_count(), 1);
}

#[tokio::test]
async fn fails_shutdown_when_an_in_flight_acquisition_cannot_release_its_harness() {
    let host = TestServerHost::new();
    host.seed("session-1");
    host.set_next_harness_close_error(ChordError::error("close failed"));
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    let gate = host.gate_next_open_session();
    let attaching = client.clone();
    let attach = tokio::spawn(async move { attaching.attach(SERVER_ID, "session-1").await });
    gate.entered.wait().await;

    let closing = server.clone();
    let closing = tokio::spawn(async move { closing.close().await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    gate.release.resolve(());

    let failure = closing.await.unwrap().unwrap_err();
    assert!(failure.to_string().contains("Failed to close routed Sessions"));
    assert!(server.closed().await.unwrap_err().to_string().contains("Failed to close routed Sessions"));
    assert!(attach.await.unwrap().unwrap_err().to_string().to_lowercase().contains("closed"));
    assert_eq!(host.latest_harness("session-1").close_count(), 1);
    RoutedSessionHandle::close(&*host.latest_harness("session-1"), &Context::background()).await.unwrap();
}
