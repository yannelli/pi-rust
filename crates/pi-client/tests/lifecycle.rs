//! Port of upstream `test/client.test.ts`, "Client connection lifecycle".

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::services::ServiceCall;
use pi_chord::{BoxFuture, json};
use pi_client::{
    ByteTransport, ByteTransportFactory, ByteTransportHandlers, Client, ClientError, ClientOptions, ConnectionState,
};
use pi_protocol::{
    AttachmentEnvelope, CborOptions, CborValue, FrameDecoderOptions, ProtocolError, ResponseEnvelope, RpcTarget,
    ServerHello, ServerHelloError, ServerMessage, SessionTarget, encode_cbor, encode_frame, encode_server_message,
};
use support::{MemoryByteServer, SERVER_ID};

fn call(member: &str) -> ServiceCall {
    ServiceCall { service_id: "test".into(), instance: None, member: member.into(), args: vec![] }
}

fn frame(message: ServerMessage) -> Vec<u8> {
    encode_server_message(&message, &FrameDecoderOptions::default()).unwrap()
}

struct CountingTransport {
    sends: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    on_send: Option<(ByteTransportHandlers, Vec<u8>)>,
}

impl ByteTransport for CountingTransport {
    fn send(&self, _chunk: Vec<u8>) -> BoxFuture<Result<(), ClientError>> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if let Some((handlers, reply)) = &self.on_send {
            handlers.on_data(reply);
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn close(&self) {
        self.closes.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn rejects_server_data_delivered_before_the_client_hello_is_sent() {
    let (sends, closes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (send_count, close_count) = (sends.clone(), closes.clone());
    let factory: ByteTransportFactory = Arc::new(move |handlers| {
        handlers.on_data(&frame(ServerMessage::Hello(ServerHello { server_id: SERVER_ID.into() })));
        let transport: Arc<dyn ByteTransport> =
            Arc::new(CountingTransport { sends: send_count.clone(), closes: close_count.clone(), on_send: None });
        Box::pin(async move { Ok(transport) })
    });
    let client = Client::new(ClientOptions::new(SERVER_ID, factory)).unwrap();
    let error = client.connect().await.unwrap_err();
    assert_eq!(error.name(), "ProtocolValidationError");
    assert_eq!(error.to_string(), "Received server data before the client hello was sent");
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    support::poll(|| closes.load(Ordering::SeqCst) == 1).await;
    assert_eq!(sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejects_typed_handshake_errors_and_closes_the_transport() {
    let closes = Arc::new(AtomicUsize::new(0));
    let close_count = closes.clone();
    let factory: ByteTransportFactory = Arc::new(move |handlers| {
        let reply = frame(ServerMessage::HelloError(ServerHelloError {
            error: ProtocolError::new("version", "Unsupported protocol version"),
        }));
        let transport: Arc<dyn ByteTransport> = Arc::new(CountingTransport {
            sends: Arc::new(AtomicUsize::new(0)),
            closes: close_count.clone(),
            on_send: Some((handlers, reply)),
        });
        Box::pin(async move { Ok(transport) })
    });
    let client = Client::new(ClientOptions::new(SERVER_ID, factory)).unwrap();
    let error = client.connect().await.unwrap_err();
    assert_eq!((error.name(), error.code()), ("ServerError", Some("version")));
    assert_eq!(error.to_string(), "Unsupported protocol version");
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejects_pending_requests_and_reconnects_through_a_fresh_transport() {
    let (first, second) = (MemoryByteServer::new(), MemoryByteServer::new());
    let connections = Arc::new(AtomicUsize::new(0));
    let (count, a, b) = (connections.clone(), first.clone(), second.clone());
    let factory: ByteTransportFactory = Arc::new(move |handlers| {
        let server = if count.fetch_add(1, Ordering::SeqCst) == 0 { &a } else { &b };
        let transport = server.connect(handlers);
        Box::pin(async move { Ok(transport) })
    });
    let client = Client::new(ClientOptions::new(SERVER_ID, factory)).unwrap();
    let states = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = states.clone();
    client
        .on_connection_state_change(Arc::new(move |change| recorded.lock().unwrap().push(change.state.to_string())))
        .unwrap()
        .forget();
    client.connect().await.unwrap();
    let target = SessionTarget {
        server_id: SERVER_ID.into(),
        session_id: "session-1".into(),
        attachment_id: "attachment-session-1".into(),
    };
    first.send(ServerMessage::Attachment(AttachmentEnvelope { attachment: Some(target.clone()) }));
    let requester = client.clone();
    let session_call = ServiceCall { service_id: "test.session".into(), ..call("run") };
    let pending = tokio::spawn(async move { requester.request(RpcTarget::Session(target), session_call, None).await });
    first.wait_for_messages(2).await;
    first.disconnect();
    assert!(matches!(pending.await.unwrap(), Err(ClientError::Disconnected { .. })));

    assert_eq!(client.reconnect().await.unwrap().server_id, SERVER_ID);
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    assert!(client.connected());
    assert_eq!(second.messages().len(), 1);
    let expected = ["connecting", "connected", "disconnected", "connecting", "connected"];
    assert_eq!(*states.lock().unwrap(), expected);
    client.dispose();
}

#[tokio::test]
async fn reports_transport_failures_without_leaving_requests_pending() {
    let server = MemoryByteServer::new();
    let client = Client::connect_new(ClientOptions::new(SERVER_ID, server.factory())).await.unwrap();
    let requester = client.clone();
    let pending =
        tokio::spawn(async move { requester.request(RpcTarget::server(SERVER_ID), call("pending"), None).await });
    server.wait_for_messages(2).await;
    server.error(ClientError::Other("read failed".into()));
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!((error.name(), error.to_string()), ("DisconnectedError", "read failed".to_string()));
    let ClientError::Disconnected { cause: Some(cause), .. } = error else { panic!("cause") };
    assert_eq!(cause.to_string(), "read failed");
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
}

#[tokio::test]
async fn disconnects_on_invalid_or_truncated_server_framing() {
    let invalid_server = MemoryByteServer::new();
    let invalid = Client::connect_new(ClientOptions::new(SERVER_ID, invalid_server.factory())).await.unwrap();
    let response = CborValue::Map(vec![
        ("type".into(), CborValue::Text("response".into())),
        ("id".into(), CborValue::Text("unknown".into())),
        ("ok".into(), CborValue::Bool(true)),
        ("result".into(), CborValue::Number(1.0)),
    ]);
    invalid_server.send_raw(&encode_frame(&encode_cbor(&response, &CborOptions::default()).unwrap()).unwrap());
    assert_eq!(invalid.connection_state(), ConnectionState::Disconnected);

    let truncated_server = MemoryByteServer::new();
    let truncated = Client::connect_new(ClientOptions::new(SERVER_ID, truncated_server.factory())).await.unwrap();
    let requester = truncated.clone();
    let pending =
        tokio::spawn(async move { requester.request(RpcTarget::server(SERVER_ID), call("pending"), None).await });
    truncated_server.wait_for_messages(2).await;
    truncated_server.send_raw(&[0, 0, 0, 2, 1]);
    truncated_server.disconnect();
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.name(), "ProtocolValidationError");
    assert!(error.to_string().to_lowercase().contains("truncated"));
    assert_eq!(truncated.connection_state(), ConnectionState::Disconnected);
}

#[tokio::test]
async fn disconnects_when_a_response_has_no_matching_request() {
    let server = MemoryByteServer::new();
    let client = Client::connect_new(ClientOptions::new(SERVER_ID, server.factory())).await.unwrap();
    server
        .send(ServerMessage::Response(ResponseEnvelope { id: "unknown-request".into(), result: Ok(Some(json!([]))) }));
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    assert_eq!(server.client_close_count(), 1);
}
