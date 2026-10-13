//! Port of upstream `test/client.test.ts`, "Client service operations".

mod support;

use std::sync::{Arc, Mutex};

use pi_chord::services::{ServiceCall, ServiceMode, ServiceProviderUpdate};
use pi_chord::{AbortController, ChordError, Context, Json, json};
use pi_client::{Client, ClientError, ClientOptions, create_client_service_transport};
use pi_protocol::{
    AttachmentEnvelope, CancelEnvelope, ClientMessage, ProtocolError, ResponseEnvelope, RpcTarget, ServerMessage,
    ServiceEventEnvelope, SessionTarget,
};
use support::{MemoryByteServer, SERVER_ID};

fn server_target() -> RpcTarget {
    RpcTarget::server(SERVER_ID)
}

fn call(service_id: &str, member: &str, args: Vec<Json>) -> ServiceCall {
    ServiceCall { service_id: service_id.into(), instance: None, member: member.into(), args }
}

pub async fn connect_client(server: &Arc<MemoryByteServer>) -> Result<Client, ClientError> {
    Client::connect_new(ClientOptions::new(SERVER_ID, server.factory())).await
}

fn respond(server: &MemoryByteServer, id: &str, result: Result<Option<Json>, ProtocolError>) {
    server.send(ServerMessage::Response(ResponseEnvelope { id: id.into(), result }));
}

async fn attach_client(client: &Client, server: &Arc<MemoryByteServer>, session_id: &str) {
    let expected = server.messages().len() + 1;
    let (requester, session) = (client.clone(), session_id.to_string());
    let attaching = tokio::spawn(async move {
        let call = call("pi.session-management", "attach", vec![json!(session)]);
        requester.request(server_target(), call, None).await
    });
    server.wait_for_messages(expected).await;
    let Some(ClientMessage::Request(request)) = server.messages().last().cloned() else { panic!("Missing attach") };
    let attachment = SessionTarget {
        server_id: SERVER_ID.into(),
        session_id: session_id.into(),
        attachment_id: format!("attachment-{session_id}"),
    };
    server.send(ServerMessage::Attachment(AttachmentEnvelope { attachment: Some(attachment) }));
    respond(server, &request.id, Ok(Some(Json::Null)));
    attaching.await.unwrap().unwrap();
}

#[tokio::test]
async fn requires_a_canonical_uuid_v4_server_identity() {
    let server = MemoryByteServer::new();
    let error = Client::new(ClientOptions::new("invalid-server", server.factory())).err().unwrap();
    assert!(error.to_string().contains("serverId"));
}

#[tokio::test]
async fn connects_only_to_the_expected_logical_server() {
    let client = connect_client(&MemoryByteServer::new()).await.unwrap();
    assert_eq!(client.hello().unwrap().server_id, SERVER_ID);
    client.dispose();

    let wrong = MemoryByteServer::with_id("00000000-0000-4000-8000-000000000002");
    assert!(matches!(connect_client(&wrong).await, Err(ClientError::Validation(_))));
}

#[tokio::test]
async fn updates_attachment_state_from_out_of_band_routing() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    let changes = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let recorded = changes.clone();
    let _guard = client
        .on_attachment_change(Arc::new(move |target| {
            recorded.lock().unwrap().push(target.map(|target| target.session_id.clone()));
        }))
        .unwrap();

    attach_client(&client, &server, "session-1").await;
    let attachment = client.attachment().unwrap();
    assert_eq!(
        (attachment.session_id.as_str(), attachment.attachment_id.as_str()),
        ("session-1", "attachment-session-1")
    );
    let ClientMessage::Request(request) = &server.messages()[1] else { panic!("request") };
    assert_eq!(request.target, server_target());
    assert_eq!(request.call, call("pi.session-management", "attach", vec![json!("session-1")]).to_json());

    server.send(ServerMessage::Attachment(AttachmentEnvelope { attachment: None }));
    assert!(client.attachment().is_none());
    assert_eq!(*changes.lock().unwrap(), vec![Some("session-1".to_string()), None]);
    client.dispose();
}

fn update(subscription_id: &str, value: Json) -> ServerMessage {
    ServerMessage::ServiceUpdate(ServiceEventEnvelope { subscription_id: subscription_id.into(), update: value })
}

#[tokio::test]
async fn buffers_service_updates_until_the_subscription_snapshot_arrives() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    attach_client(&client, &server, "session-1").await;
    let target = client.attachment().unwrap();
    let resolver = client.clone();
    let transport = create_client_service_transport(&client, Arc::new(move || resolver.attachment().map(Into::into)));
    let updates = Arc::new(Mutex::new(Vec::<ServiceProviderUpdate>::new()));
    let recorded = updates.clone();
    let listener = Arc::new(move |update: ServiceProviderUpdate, _: &Context| {
        recorded.lock().unwrap().push(update);
        Ok(())
    });
    let subscriber = transport.clone();
    let opening = tokio::spawn(async move {
        subscriber.subscribe("pi.models", ServiceMode::Singleton, listener, &Context::background()).await
    });
    server.wait_for_messages(3).await;
    let ClientMessage::Request(request) = &server.messages()[2] else { panic!("subscribe request") };
    assert_eq!(request.target, RpcTarget::Session(target));
    assert_eq!(
        request.call,
        json!({ "serviceId": "$chord.service", "member": "subscribe",
        "args": ["service-1", "pi.models", "singleton"] })
    );

    server.send(update(
        "service-1",
        json!({ "type": "state", "member": "state", "sequence": 1,
        "ops": [["s", ["revision"], 1]] }),
    ));
    tokio::task::yield_now().await;
    assert!(updates.lock().unwrap().is_empty());
    respond(
        &server,
        "request-2",
        Ok(Some(json!({ "serviceId": "pi.models", "mode": "singleton",
        "instances": [{ "members": [{ "name": "state", "kind": "state", "sequence": 0,
            "ops": [["r", { "revision": 0 }]] }] }] }))),
    );
    let subscription = opening.await.unwrap().unwrap();
    assert!(updates.lock().unwrap().is_empty());
    server.send(update(
        "closed-subscription",
        json!({ "type": "state", "member": "state",
        "sequence": 99, "ops": [["s", 99, 99]] }),
    ));
    assert!(client.connected());
    let members: Vec<Json> = subscription.snapshot().instances[0].members.iter().map(|m| m.to_json()).collect();
    assert_eq!(
        members,
        vec![json!({ "name": "state", "kind": "state", "sequence": 0, "ops": [["r", { "revision": 0 }]] })]
    );

    subscription.activate().unwrap();
    assert_eq!(updates.lock().unwrap().iter().map(|u| u.type_name()).collect::<Vec<_>>(), vec!["state"]);
    server.send(update(
        "service-1",
        json!({ "type": "state", "member": "state", "sequence": 2,
        "ops": [["s", ["revision"], 2]] }),
    ));
    server.send(update(
        "service-1",
        json!({ "type": "state", "member": "state", "sequence": 3,
        "ops": [["#", 0, ["revision"]], ["s", 0, 3]] }),
    ));
    assert_eq!(updates.lock().unwrap().len(), 3);
    assert_eq!(updates.lock().unwrap()[2].to_json()["ops"], json!([["s", ["revision"], 3]]));

    let closing = tokio::spawn(async move { subscription.close(&Context::background()).await });
    server.wait_for_messages(4).await;
    let ClientMessage::Request(request) = &server.messages()[3] else { panic!("unsubscribe request") };
    assert_eq!(request.call, json!({ "serviceId": "$chord.service", "member": "unsubscribe", "args": ["service-1"] }));
    respond(&server, "request-3", Ok(None));
    closing.await.unwrap().unwrap();
    client.dispose();
}

#[tokio::test]
async fn correlates_out_of_order_generic_service_responses() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    let (a, b) = (client.clone(), client.clone());
    let first = tokio::spawn(async move { a.request(server_target(), call("test", "first", vec![]), None).await });
    server.wait_for_messages(2).await;
    let second = tokio::spawn(async move { b.request(server_target(), call("test", "second", vec![]), None).await });
    server.wait_for_messages(3).await;
    respond(&server, "request-2", Ok(Some(json!("second"))));
    respond(&server, "request-1", Ok(Some(json!("first"))));
    assert_eq!(first.await.unwrap().unwrap(), Some(json!("first")));
    assert_eq!(second.await.unwrap().unwrap(), Some(json!("second")));
    client.dispose();
}

#[tokio::test]
async fn exposes_bounded_server_errors() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    let requester = client.clone();
    let pending =
        tokio::spawn(async move { requester.request(server_target(), call("test", "missing", vec![]), None).await });
    server.wait_for_messages(2).await;
    respond(&server, "request-1", Err(ProtocolError::new("session_not_found", "Unknown session")));
    assert_eq!(pending.await.unwrap().unwrap_err().code(), Some("session_not_found"));
    client.dispose();
}

#[tokio::test]
async fn does_not_send_a_pre_aborted_request() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    let controller = AbortController::new();
    controller.abort(Some(ChordError::error("already cancelled")));
    let result = client.request(server_target(), call("test", "noop", vec![]), Some(controller.signal())).await;
    assert_eq!(result.unwrap_err().to_string(), "already cancelled");
    assert_eq!(server.messages().len(), 1);
    client.dispose();
}

#[tokio::test]
async fn cancels_one_request_without_disconnecting() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    let controller = AbortController::new();
    let (requester, signal) = (client.clone(), controller.signal());
    let pending = tokio::spawn(async move {
        requester.request(server_target(), call("test", "mutate", vec![json!({ "value": 42 })]), Some(signal)).await
    });
    server.wait_for_messages(2).await;
    let ClientMessage::Request(request) = &server.messages()[1] else { panic!("request") };
    assert_eq!(request.id, "request-1");

    controller.abort(Some(ChordError::error("stop this request")));
    assert_eq!(pending.await.unwrap().unwrap_err().to_string(), "stop this request");
    server.wait_for_messages(3).await;
    let cancel = CancelEnvelope { id: "request-1".into(), target: server_target() };
    assert_eq!(server.messages()[2], ClientMessage::Cancel(cancel));
    respond(&server, "request-1", Err(ProtocolError::new("cancelled", "cancelled")));
    assert!(client.connected());
    client.dispose();
}

#[tokio::test]
async fn rejects_pending_requests_after_disconnect_or_disposal() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    let requester = client.clone();
    let pending =
        tokio::spawn(async move { requester.request(server_target(), call("test", "pending", vec![]), None).await });
    server.wait_for_messages(2).await;
    server.disconnect();
    assert!(matches!(pending.await.unwrap(), Err(ClientError::Disconnected { .. })));
    client.dispose();
    let result = client.request(server_target(), call("test", "disposed", vec![]), None).await;
    assert!(matches!(result, Err(ClientError::Disposed)));
}

#[tokio::test]
async fn rejects_an_invalid_service_call_before_sending() {
    let server = MemoryByteServer::new();
    let client = connect_client(&server).await.unwrap();
    let result = client.request(server_target(), call("", "member", vec![]), None).await;
    assert!(matches!(result, Err(ClientError::Type(_))));
    assert_eq!(server.messages().len(), 1);
    client.dispose();
}
