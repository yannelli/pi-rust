//! Port of upstream `test/conformance.test.ts`, "Session protocol".

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{SERVER_ID, assert_error, assert_ok, call, latest_attachment_id, poll_until, session_call};
use pi_chord::services::{ServiceCall, ServiceUpdatePublisher};
use pi_chord::{ChordError, ChordResult, Context, Json, json};
use pi_protocol::{ClientMessage, PROTOCOL_VERSION, RequestEnvelope, RpcTarget, ServerMessage, SessionTarget};
use pi_server::testing::{TestServerHost, connect_in_memory, create_test_server_services};
use pi_server::{
    BasicSessionMetadata, RoutedServerPresentation, RoutedServerServiceAttachment, RoutedServerServiceHost,
    RoutedSessionAttachment, RoutedSessionHandle, Server, ServerError, ServerHost, ServerOptions, SessionMetadata,
};

const VERSION: u64 = PROTOCOL_VERSION as u64;

fn server<H: ServerHost>(host: Arc<H>) -> Server<H> {
    Server::new(host, ServerOptions::new(SERVER_ID, vec![])).unwrap()
}

#[tokio::test]
async fn handshake_identifies_the_server_without_listing_sessions() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    let reply = client.hello(VERSION).await.unwrap();
    assert!(matches!(reply, ServerMessage::Hello(ref hello) if hello.server_id == SERVER_ID));
    assert_eq!(host.harness_session_count(), 0);
    server.close().await.unwrap();
}

#[tokio::test]
async fn rejects_a_semantically_invalid_service_call() {
    let server = server(TestServerHost::new());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    let request = RequestEnvelope {
        id: "invalid-call".into(),
        target: RpcTarget::server(SERVER_ID),
        call: json!({ "arbitrary": true }),
    };
    client.send_message(ClientMessage::Request(request)).await.unwrap();
    let response = client.next(|m| matches!(m, ServerMessage::Response(r) if r.id == "invalid-call")).await.unwrap();
    let ServerMessage::Response(response) = response else { unreachable!() };
    assert_error(&response, "invalid_request");
    server.close().await.unwrap();
}

#[derive(Clone, Debug, PartialEq)]
struct BackendMetadata {
    id: String,
    path: String,
    modified_at: u64,
}

impl SessionMetadata for BackendMetadata {
    fn id(&self) -> &str {
        &self.id
    }
}

struct VoidAttachment;

#[async_trait]
impl RoutedSessionAttachment for VoidAttachment {
    async fn invoke_service(
        &self,
        _: ServiceCall,
        _: ServiceUpdatePublisher,
        _: &Context,
    ) -> ChordResult<Option<Json>> {
        Ok(None)
    }
    async fn release(&self, _: &Context) -> ChordResult<()> {
        Ok(())
    }
}

struct VoidHandle;

#[async_trait]
impl RoutedSessionHandle for VoidHandle {
    async fn attach_client(&self, _: &Context) -> ChordResult<Arc<dyn RoutedSessionAttachment>> {
        Ok(Arc::new(VoidAttachment))
    }
    async fn close(&self, _: &Context) -> ChordResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct MetadataHost {
    received: Mutex<Option<BackendMetadata>>,
    ambiguous: bool,
}

#[async_trait]
impl ServerHost for MetadataHost {
    type Metadata = BackendMetadata;
    fn server_services(&self) -> Arc<dyn RoutedServerServiceHost> {
        create_test_server_services()
    }
    async fn resolve_session(&self, id: &str, _: &Context) -> ChordResult<BackendMetadata> {
        if self.ambiguous {
            return Err(ServerError::session_ambiguous().into());
        }
        Ok(BackendMetadata { id: id.into(), path: format!("/sessions/{id}.jsonl"), modified_at: 2 })
    }
    async fn open_session(&self, metadata: BackendMetadata, _: &Context) -> ChordResult<Arc<dyn RoutedSessionHandle>> {
        assert!(!self.ambiguous, "must not create a Harness for an ambiguous session");
        *self.received.lock().unwrap() = Some(metadata);
        Ok(Arc::new(VoidHandle))
    }
}

#[tokio::test]
async fn attach_passes_concrete_repository_metadata_to_the_host() {
    let host = Arc::new(MetadataHost::default());
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    assert_ok(&client.attach(SERVER_ID, "session-1").await.unwrap());
    let expected = BackendMetadata { id: "session-1".into(), path: "/sessions/session-1.jsonl".into(), modified_at: 2 };
    assert_eq!(host.received.lock().unwrap().clone(), Some(expected));
    server.close().await.unwrap();
}

#[tokio::test]
async fn rejects_an_ambiguous_session_id_without_creating_a_harness() {
    let server = server(Arc::new(MetadataHost { ambiguous: true, ..MetadataHost::default() }));
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    assert_error(&client.attach(SERVER_ID, "duplicate").await.unwrap(), "session_ambiguous");
    server.close().await.unwrap();
}

struct CountingServices {
    releases: Arc<Mutex<usize>>,
}

struct CountingAttachment {
    presentation: RoutedServerPresentation,
    releases: Arc<Mutex<usize>>,
}

#[async_trait]
impl RoutedServerServiceAttachment for CountingAttachment {
    async fn invoke_service(
        &self,
        call: ServiceCall,
        _: ServiceUpdatePublisher,
        ctx: &Context,
    ) -> ChordResult<Option<Json>> {
        assert_eq!(call.service_id, "pi.session-management");
        match call.member.as_str() {
            "attach" => self.presentation.attach_session(call.args[0].as_str().unwrap(), ctx).await?,
            "detach" => self.presentation.detach_session(ctx).await?,
            _ => return Err(ChordError::error("Unexpected service member")),
        }
        Ok(Some(Json::Null))
    }
    async fn release(&self, _: &Context) -> ChordResult<()> {
        *self.releases.lock().unwrap() += 1;
        Ok(())
    }
}

#[async_trait]
impl RoutedServerServiceHost for CountingServices {
    async fn attach_client(
        &self,
        presentation: RoutedServerPresentation,
        _: &Context,
    ) -> ChordResult<Arc<dyn RoutedServerServiceAttachment>> {
        Ok(Arc::new(CountingAttachment { presentation, releases: self.releases.clone() }))
    }
}

struct DelegatingHost {
    backing: Arc<TestServerHost>,
    services: Arc<dyn RoutedServerServiceHost>,
}

#[async_trait]
impl ServerHost for DelegatingHost {
    type Metadata = BasicSessionMetadata;
    fn server_services(&self) -> Arc<dyn RoutedServerServiceHost> {
        self.services.clone()
    }
    async fn resolve_session(&self, id: &str, context: &Context) -> ChordResult<BasicSessionMetadata> {
        self.backing.resolve_session(id, context).await
    }
    async fn open_session(
        &self,
        metadata: BasicSessionMetadata,
        ctx: &Context,
    ) -> ChordResult<Arc<dyn RoutedSessionHandle>> {
        self.backing.open_session(metadata, ctx).await
    }
}

#[tokio::test]
async fn routes_opaque_server_services_and_publishes_attachment_changes() {
    let backing = TestServerHost::new();
    backing.seed("session-1");
    let releases = Arc::new(Mutex::new(0));
    let services = Arc::new(CountingServices { releases: releases.clone() });
    let server = server(Arc::new(DelegatingHost { backing: backing.clone(), services }));
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    assert_ok(&client.attach(SERVER_ID, "session-1").await.unwrap());
    let attached = client
        .next(|m| matches!(m, ServerMessage::Attachment(envelope) if envelope.attachment.is_some()))
        .await
        .unwrap();
    let ServerMessage::Attachment(envelope) = attached else { unreachable!() };
    assert_eq!(envelope.attachment.unwrap().session_id, "session-1");
    assert_eq!(backing.latest_harness("session-1").attached_clients(), 1);

    let detach = call("pi.session-management", "detach");
    assert_ok(&client.request_service(RpcTarget::server(SERVER_ID), detach, None).await.unwrap());
    client.next(|m| matches!(m, ServerMessage::Attachment(envelope) if envelope.attachment.is_none())).await.unwrap();
    assert_eq!(backing.latest_harness("session-1").attached_clients(), 0);
    client.close().await;
    poll_until(|| *releases.lock().unwrap() == 1).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn permits_multiple_client_attachments_per_session() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let (first, second) = (connect_in_memory(&server), connect_in_memory(&server));
    first.hello(VERSION).await.unwrap();
    second.hello(VERSION).await.unwrap();
    assert_ok(&first.attach(SERVER_ID, "session-1").await.unwrap());
    assert_ok(&first.attach(SERVER_ID, "session-1").await.unwrap());
    assert_eq!(host.latest_harness("session-1").attached_clients(), 1);
    assert_ok(&second.attach(SERVER_ID, "session-1").await.unwrap());
    assert_eq!(host.harnesses("session-1").len(), 1);
    assert_eq!(host.latest_harness("session-1").attached_clients(), 2);

    first.close().await;
    poll_until(|| host.latest_harness("session-1").attached_clients() == 1).await;
    assert_ok(&second.attach(SERVER_ID, "session-1").await.unwrap());
    server.close().await.unwrap();
}

#[tokio::test]
async fn clears_connection_ownership_when_attachment_release_fails() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = errors.clone();
    let mut options = ServerOptions::new(SERVER_ID, vec![]);
    options.on_error = Some(Arc::new(move |error| recorded.lock().unwrap().push(error.to_string())));
    let server = Server::new(host.clone(), options).unwrap();
    let (first, second) = (connect_in_memory(&server), connect_in_memory(&server));
    first.hello(VERSION).await.unwrap();
    second.hello(VERSION).await.unwrap();
    first.attach(SERVER_ID, "session-1").await.unwrap();
    let harness = host.latest_harness("session-1");
    harness.set_fail_attachment_release(Some(ChordError::error("release failed")));

    first.close().await;
    poll_until(|| harness.attachment_release_count() == 1).await;
    poll_until(|| errors.lock().unwrap().iter().any(|error| error == "release failed")).await;
    harness.set_fail_attachment_release(None);
    assert_ok(&second.attach(SERVER_ID, "session-1").await.unwrap());
    server.close().await.unwrap();
}

#[tokio::test]
async fn requires_the_requesting_client_to_hold_the_targeted_attachment() {
    let host = TestServerHost::new();
    host.seed("session-1");
    host.seed("session-2");
    let server = server(host.clone());
    let (attached, unattached) = (connect_in_memory(&server), connect_in_memory(&server));
    attached.hello(VERSION).await.unwrap();
    unattached.hello(VERSION).await.unwrap();
    let response = unattached.request_session_service(SERVER_ID, "session-1", session_call("run", vec![]), None);
    assert_error(&response.await.unwrap(), "session_not_attached");
    attached.attach(SERVER_ID, "session-1").await.unwrap();
    let response = attached.request_session_service(SERVER_ID, "session-2", session_call("run", vec![]), None);
    assert_error(&response.await.unwrap(), "session_not_attached");
    let hello_call = session_call("run", vec![json!("Hello")]);
    let response = attached.request_session_service(SERVER_ID, "session-1", hello_call.clone(), None).await.unwrap();
    assert_eq!(response.result, Ok(Some(json!({ "ok": true }))));
    assert_eq!(host.latest_harness("session-1").service_calls(), vec![hello_call]);
    server.close().await.unwrap();
}

#[tokio::test]
async fn rejects_a_stale_attachment_route_after_switching_sessions() {
    let host = TestServerHost::new();
    host.seed("session-1");
    host.seed("session-2");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    client.attach(SERVER_ID, "session-1").await.unwrap();
    let first_attachment_id = latest_attachment_id(&client, "session-1");
    client.attach(SERVER_ID, "session-2").await.unwrap();
    let stale = SessionTarget {
        server_id: SERVER_ID.into(),
        session_id: "session-1".into(),
        attachment_id: first_attachment_id,
    };
    let response = client.request_service(stale.into(), session_call("run", vec![json!("stale")]), None);
    assert_error(&response.await.unwrap(), "session_not_attached");
    assert!(host.latest_harness("session-1").service_calls().is_empty());
    server.close().await.unwrap();
}

#[tokio::test]
async fn preserves_opaque_results_and_bounds_adapter_defects() {
    let host = TestServerHost::new();
    host.seed("session-1");
    let server = server(host.clone());
    let client = connect_in_memory(&server);
    client.hello(VERSION).await.unwrap();
    client.attach(SERVER_ID, "session-1").await.unwrap();
    let harness = host.latest_harness("session-1");
    harness.set_next_service_result(Some(json!({ "accepted": false, "reason": "closed" })));
    let response = client.request_session_service(SERVER_ID, "session-1", session_call("run", vec![]), None);
    assert_eq!(response.await.unwrap().result, Ok(Some(json!({ "accepted": false, "reason": "closed" }))));

    harness.set_next_service_error(ChordError::error("private adapter detail"));
    let response = client.request_session_service(SERVER_ID, "session-1", session_call("run", vec![]), None);
    let error = response.await.unwrap().result.unwrap_err();
    assert_eq!((error.code.as_str(), error.message.as_str()), ("internal_error", "Internal server error"));
    server.close().await.unwrap();
}
