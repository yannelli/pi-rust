//! Port of upstream `test/protocol.test.ts`.

mod common;

use std::sync::{Arc, Mutex};

use common::{SERVER_ID, assert_error, call, hello_error_code};
use pi_chord::{BoxFuture, ChordError, ChordResult};
use pi_protocol::{
    CborOptions, CborValue, ClientHello, ClientMessage, FrameDecoderOptions, PROTOCOL_VERSION, RequestEnvelope,
    RpcTarget, ServerMessage, encode_cbor, encode_client_message, encode_frame,
};
use pi_server::testing::{TestServerHost, connect_in_memory};
use pi_server::{ByteConnection, Server, ServerOptions};

fn server() -> Server<TestServerHost> {
    Server::new(TestServerHost::new(), ServerOptions::new(SERVER_ID, vec![])).unwrap()
}

fn hello() -> ClientMessage {
    ClientMessage::Hello(ClientHello::default())
}

fn list_request(id: &str) -> ClientMessage {
    ClientMessage::Request(RequestEnvelope {
        id: id.into(),
        target: RpcTarget::server(SERVER_ID),
        call: call("pi.session-directory", "list").to_json(),
    })
}

fn is_hello_error(message: &ServerMessage) -> bool {
    matches!(message, ServerMessage::HelloError(_))
}

#[tokio::test]
async fn requires_hello_as_the_first_message() {
    let server = server();
    let client = connect_in_memory(&server);
    client.send_message(list_request("request-1")).await.unwrap();
    let error = client.next(is_hello_error).await.unwrap();
    assert_eq!(hello_error_code(&error), Some("invalid_request"));
    client.wait_for_close().await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn rejects_unsupported_protocol_versions() {
    let server = server();
    let client = connect_in_memory(&server);
    let reply = client.hello(u64::from(PROTOCOL_VERSION) + 1).await.unwrap();
    assert_eq!(hello_error_code(&reply), Some("version"));
    client.wait_for_close().await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn accepts_fragmented_hello_and_request_frames() {
    let server = server();
    let client = connect_in_memory(&server);
    let length = encode_client_message(&hello(), &FrameDecoderOptions::default()).unwrap().len();
    client.send_fragmented_message(hello(), length / 2).await.unwrap();
    let reply = client.next(|message| matches!(message, ServerMessage::Hello(_))).await.unwrap();
    assert!(matches!(reply, ServerMessage::Hello(ref hello) if hello.server_id == SERVER_ID));

    let length = encode_client_message(&list_request("request-1"), &FrameDecoderOptions::default()).unwrap().len();
    client.send_fragmented_message(list_request("request-1"), length / 2).await.unwrap();
    let response = client.next(|message| matches!(message, ServerMessage::Response(_))).await.unwrap();
    let ServerMessage::Response(response) = response else { unreachable!() };
    assert_error(&response, "internal_error");
    server.close().await.unwrap();
}

#[tokio::test]
async fn rejects_hostile_framed_input() {
    let schema_invalid = CborValue::Map(vec![
        ("type".into(), CborValue::Text("hello".into())),
        ("version".into(), CborValue::Number(1.0)),
        ("extra".into(), CborValue::Bool(true)),
    ]);
    let cases = [
        encode_frame(&[0xff]).unwrap(),
        encode_frame(&encode_cbor(&schema_invalid, &CborOptions::default()).unwrap()).unwrap(),
        vec![1, 0, 0, 1],
    ];
    for bytes in cases {
        let server = server();
        let client = connect_in_memory(&server);
        client.send_bytes(bytes).await.unwrap();
        let error = client.next(is_hello_error).await.unwrap();
        assert_eq!(hello_error_code(&error), Some("invalid_request"));
        client.wait_for_close().await;
        server.close().await.unwrap();
    }
}

#[tokio::test]
async fn rejects_a_second_hello_after_the_handshake() {
    let server = server();
    let client = connect_in_memory(&server);
    client.hello(u64::from(PROTOCOL_VERSION)).await.unwrap();
    client.send_message(hello()).await.unwrap();
    let ServerMessage::HelloError(error) = client.next(is_hello_error).await.unwrap() else { unreachable!() };
    assert_eq!(error.error.code, "invalid_request");
    assert!(error.error.message.contains("first message"));
    client.wait_for_close().await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn processes_a_hello_and_request_coalesced_in_one_chunk() {
    let server = server();
    let client = connect_in_memory(&server);
    let options = FrameDecoderOptions::default();
    let wire = [
        encode_client_message(&hello(), &options).unwrap(),
        encode_client_message(&list_request("request-1"), &options).unwrap(),
    ]
    .concat();
    client.send_bytes(wire).await.unwrap();
    client.next(|message| matches!(message, ServerMessage::Hello(_))).await.unwrap();
    let ServerMessage::Response(response) =
        client.next(|message| matches!(message, ServerMessage::Response(_))).await.unwrap()
    else {
        unreachable!()
    };
    assert_eq!(response.id, "request-1");
    assert_error(&response, "internal_error");
    server.close().await.unwrap();
}

struct SilentConnection {
    closed: Mutex<bool>,
}

impl ByteConnection for SilentConnection {
    fn closed(&self) -> bool {
        *self.closed.lock().unwrap()
    }

    fn send(&self, _chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn close(&self, _final_chunk: Option<Vec<u8>>) -> BoxFuture<ChordResult<()>> {
        *self.closed.lock().unwrap() = true;
        Box::pin(std::future::ready(Ok(())))
    }
}

#[tokio::test]
async fn reports_a_truncated_final_frame_when_the_peer_closes() {
    let errors = Arc::new(Mutex::new(Vec::<ChordError>::new()));
    let recorded = errors.clone();
    let mut options = ServerOptions::new(SERVER_ID, vec![]);
    options.on_error = Some(Arc::new(move |error| recorded.lock().unwrap().push(error.clone())));
    let server = Server::new(TestServerHost::new(), options).unwrap();
    let connection = Arc::new(SilentConnection { closed: Mutex::new(false) });
    let handler = server.accept(connection.clone());
    handler.on_data(&[0, 0, 0, 2, 1]);
    handler.on_close();

    assert!(!connection.closed());
    let errors = errors.lock().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].to_string().to_lowercase().contains("truncated"));
}
