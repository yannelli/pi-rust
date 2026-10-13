//! Port of upstream `test/protocol.test.ts`, plus golden frames for upstream-built messages.

use pi_chord::{Json, json};
use pi_protocol::{
    CborOptions, CborValue, ClientHello, ClientMessage, ClientMessageDecoder, FrameDecoder, FrameDecoderOptions,
    PROTOCOL_VERSION, ProtocolError, ResponseEnvelope, RpcTarget, ServerHello, ServerMessage, ServerMessageDecoder,
    decode_json_cbor, encode_cbor, encode_client_message, encode_frame, encode_server_message,
    is_supported_protocol_version, parse_client_message, parse_server_message,
};

const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

fn hex(parts: &[&str]) -> Vec<u8> {
    let joined: String = parts.concat();
    (0..joined.len()).step_by(2).map(|index| u8::from_str_radix(&joined[index..index + 2], 16).unwrap()).collect()
}

fn ascii(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn defaults() -> FrameDecoderOptions {
    FrameDecoderOptions::default()
}

fn server_hello() -> ServerMessage {
    ServerMessage::Hello(ServerHello { server_id: SERVER_ID.into() })
}

#[test]
fn negotiates_protocol_version_8() {
    assert_eq!(PROTOCOL_VERSION, 8);
    assert!(is_supported_protocol_version(8));
    assert!(!is_supported_protocol_version(7));
}

#[test]
fn golden_hello_frames_match_upstream_bytes() {
    let client = encode_client_message(&ClientMessage::Hello(ClientHello::default()), &defaults()).unwrap();
    let expected = hex(&["00000015", "a2", "64", &ascii("type"), "65", &ascii("hello"), "67", &ascii("version"), "08"]);
    assert_eq!(client, expected);

    let server = encode_server_message(&server_hello(), &defaults()).unwrap();
    let expected = hex(&[
        "00000044",
        "a3",
        "64",
        &ascii("type"),
        "65",
        &ascii("hello"),
        "67",
        &ascii("version"),
        "08",
        "68",
        &ascii("serverId"),
        "7824",
        &ascii(SERVER_ID),
    ]);
    assert_eq!(server, expected);
    let mut decoder = ServerMessageDecoder::default();
    assert_eq!(decoder.push(&expected).unwrap(), vec![server_hello()]);
}

#[test]
fn golden_void_response_omits_result() {
    let response = ServerMessage::Response(ResponseEnvelope { id: "r".into(), result: Ok(None) });
    let bytes = encode_server_message(&response, &defaults()).unwrap();
    let expected =
        hex(&["00000018", "a3", "64", &ascii("type"), "68", &ascii("response"), "62", &ascii("id"), "61", "72"]);
    assert_eq!(&bytes[..expected.len()], &expected[..]);
    assert_eq!(&bytes[expected.len()..], &hex(&["62", &ascii("ok"), "f5"])[..]);
}

#[test]
fn accepts_integer_client_hello_versions_for_negotiation() {
    for version in [0, 8, 9] {
        let value = json!({ "type": "hello", "version": version });
        assert_eq!(parse_client_message(&value).unwrap(), ClientMessage::Hello(ClientHello { version }));
    }
}

#[test]
fn rejects_invalid_client_hellos() {
    for value in [
        json!({ "type": "hello", "version": "8" }),
        json!({ "type": "hello", "version": 8.5 }),
        json!({ "type": "hello", "version": 8, "extra": true }),
    ] {
        assert!(parse_client_message(&value).is_err(), "{value}");
    }
}

#[test]
fn rejects_non_canonical_uuid_v4_server_ids() {
    for server_id in [
        "",
        "server-1",
        "00000000-0000-7000-8000-000000000001",
        "00000000-0000-4000-7000-000000000001",
        "00000000-0000-4000-8000-00000000000A",
    ] {
        let value = json!({
            "type": "request", "id": "request-1", "target": { "serverId": server_id },
            "call": { "serviceId": "pi.models", "member": "list", "args": [] },
        });
        assert!(parse_client_message(&value).is_err(), "{server_id}");
    }
}

#[test]
fn keeps_routed_request_and_event_payloads_opaque() {
    let call = json!({
        "serviceId": "application.custom", "instance": { "key": "instance-1", "generation": 2 },
        "member": "invoke", "args": [{ "arbitrary": true }, ["opaque"]],
    });
    let value = json!({
        "type": "request", "id": "request-1",
        "target": { "serverId": SERVER_ID, "sessionId": "session-1", "attachmentId": "attachment-1" },
        "call": call.to_serde(),
    });
    let ClientMessage::Request(request) = parse_client_message(&value).unwrap() else { panic!("request") };
    assert_eq!(request.call, call);
    assert_eq!(request.target.session().unwrap().attachment_id, "attachment-1");
    assert_eq!(ClientMessage::Request(request).to_json(), value);

    let update = json!({ "type": "service_update", "subscriptionId": "subscription-1", "update": { "x": true } });
    assert!(matches!(parse_server_message(&update).unwrap(), ServerMessage::ServiceUpdate(_)));
}

#[test]
fn rejects_non_json_opaque_payloads_on_the_wire() {
    let request = CborValue::Map(vec![
        ("type".into(), CborValue::Text("request".into())),
        ("id".into(), CborValue::Text("request-1".into())),
        ("target".into(), CborValue::Map(vec![("serverId".into(), CborValue::Text(SERVER_ID.into()))])),
        ("call".into(), CborValue::Bytes(vec![1])),
    ]);
    let wire = encode_frame(&encode_cbor(&request, &CborOptions::default()).unwrap()).unwrap();
    let mut decoder = ClientMessageDecoder::default();
    assert!(decoder.push(&wire).is_err());
}

#[test]
fn validates_request_cancellation_envelopes() {
    let cancel = json!({ "type": "cancel", "id": "request-1", "target": { "serverId": SERVER_ID } });
    assert!(matches!(parse_client_message(&cancel).unwrap(), ClientMessage::Cancel(_)));
    let empty = json!({ "type": "cancel", "id": "", "target": { "serverId": SERVER_ID } });
    assert!(parse_client_message(&empty).is_err());
    let extra = json!({ "type": "cancel", "id": "r", "target": { "serverId": SERVER_ID }, "extra": true });
    assert!(parse_client_message(&extra).is_err());
}

#[test]
fn validates_attachment_route_updates() {
    let attached = json!({ "type": "attachment",
        "attachment": { "serverId": SERVER_ID, "sessionId": "session-1", "attachmentId": "attachment-1" } });
    let detached = json!({ "type": "attachment", "attachment": null });
    assert_eq!(parse_server_message(&attached).unwrap().to_json(), attached);
    assert_eq!(parse_server_message(&detached).unwrap().to_json(), detached);
    let partial = json!({ "type": "attachment", "attachment": { "sessionId": "session-1" } });
    assert!(parse_server_message(&partial).is_err());
}

#[test]
fn rejects_malformed_request_boundaries() {
    let call = json!({ "serviceId": "pi.models", "member": "list", "args": [] }).to_serde();
    let empty_id = json!({ "type": "request", "id": "", "target": { "serverId": SERVER_ID }, "call": call });
    let extra = json!({ "type": "request", "id": "r", "target": { "serverId": SERVER_ID }, "call": call, "x": 1 });
    assert!(parse_client_message(&empty_id).is_err());
    assert!(parse_client_message(&extra).is_err());
}

#[test]
fn accepts_a_successful_void_response_without_a_result() {
    let value = json!({ "type": "response", "id": "request-1", "ok": true });
    let parsed = parse_server_message(&value).unwrap();
    assert_eq!(parsed, ServerMessage::Response(ResponseEnvelope { id: "request-1".into(), result: Ok(None) }));
}

#[test]
fn rejects_malformed_server_boundaries() {
    for value in [
        json!({ "type": "hello", "version": 8, "serverId": "server-1" }),
        json!({ "type": "response", "id": "request-1", "ok": true, "result": [], "extra": true }),
        json!({ "type": "response", "id": "request-1", "ok": false, "error": { "code": "", "message": "bad" } }),
        json!({ "type": "hello", "version": 8, "serverId": SERVER_ID, "snapshot": {} }),
        json!({ "type": "unknown", "event": {} }),
    ] {
        assert!(parse_server_message(&value).is_err(), "{value}");
    }
}

#[test]
fn accepts_opaque_error_codes() {
    for code in ["wrong_server", "cancelled", "service_not_found", "application_error"] {
        let message = ServerMessage::Response(ResponseEnvelope {
            id: "request-1".into(),
            result: Err(ProtocolError::new(code, "safe")),
        });
        assert_eq!(parse_server_message(&message.to_json()).unwrap(), message);
    }
}

#[test]
fn does_not_parse_json_strings_as_messages() {
    let text = Json::from(r#"{"type":"hello","version":8}"#);
    assert!(parse_client_message(&text).is_err());
    assert!(parse_server_message(&text).is_err());
}

#[test]
fn encodes_complete_client_and_server_frames() {
    let hello = ClientMessage::Hello(ClientHello::default());
    let frames = FrameDecoder::default().push(&encode_client_message(&hello, &defaults()).unwrap()).unwrap();
    let decoded = decode_json_cbor(&frames[0], &CborOptions::default()).unwrap();
    assert_eq!(parse_client_message(&decoded).unwrap(), hello);
    let frames = FrameDecoder::default().push(&encode_server_message(&server_hello(), &defaults()).unwrap()).unwrap();
    let decoded = decode_json_cbor(&frames[0], &CborOptions::default()).unwrap();
    assert_eq!(parse_server_message(&decoded).unwrap(), server_hello());
}

#[test]
fn enforces_outbound_frame_limits_and_validation() {
    let small = FrameDecoderOptions::with_max_frame_length(8);
    assert!(encode_client_message(&ClientMessage::Hello(ClientHello::default()), &small).is_err());
    assert!(encode_server_message(&server_hello(), &small).is_err());
    let invalid = ServerMessage::Hello(ServerHello { server_id: "server-1".into() });
    assert!(encode_server_message(&invalid, &defaults()).is_err());
}

#[test]
fn incrementally_decodes_fragmented_and_coalesced_client_messages() {
    let hello = ClientMessage::Hello(ClientHello::default());
    let request = parse_client_message(&json!({
        "type": "request", "id": "request-1", "target": { "serverId": SERVER_ID },
        "call": { "serviceId": "pi.session-directory", "member": "list", "args": [] },
    }))
    .unwrap();
    let wire =
        [encode_client_message(&hello, &defaults()).unwrap(), encode_client_message(&request, &defaults()).unwrap()]
            .concat();
    for split in 0..=wire.len() {
        let mut decoder = ClientMessageDecoder::default();
        let mut messages = decoder.push(&wire[..split]).unwrap();
        messages.extend(decoder.push(&wire[split..]).unwrap());
        decoder.end().unwrap();
        assert_eq!(messages, vec![hello.clone(), request.clone()]);
    }
    assert!(matches!(request, ClientMessage::Request(ref envelope) if envelope.target == RpcTarget::server(SERVER_ID)));
}

#[test]
fn incrementally_decodes_fragmented_and_coalesced_server_messages() {
    let response = ServerMessage::Response(ResponseEnvelope { id: "request-1".into(), result: Ok(Some(json!([]))) });
    let first = encode_server_message(&server_hello(), &defaults()).unwrap();
    let second = encode_server_message(&response, &defaults()).unwrap();
    let wire = [first.clone(), second.clone()].concat();
    let split = first.len() + second.len() / 2;
    let mut decoder = ServerMessageDecoder::default();
    assert_eq!(decoder.push(&wire[..split]).unwrap(), vec![server_hello()]);
    assert_eq!(decoder.push(&wire[split..]).unwrap(), vec![response]);
    decoder.end().unwrap();
}

#[test]
fn rejects_invalid_framed_input_and_stays_failed() {
    let schema_invalid = CborValue::Map(vec![
        ("type".into(), CborValue::Text("hello".into())),
        ("version".into(), CborValue::Number(1.0)),
        ("extra".into(), CborValue::Bool(true)),
    ]);
    let cases = [
        encode_frame(&[]).unwrap(),
        encode_frame(&[0xff]).unwrap(),
        encode_frame(&encode_cbor(&schema_invalid, &CborOptions::default()).unwrap()).unwrap(),
    ];
    let hello = encode_client_message(&ClientMessage::Hello(ClientHello::default()), &defaults()).unwrap();
    for wire in cases {
        let mut decoder = ClientMessageDecoder::default();
        assert!(decoder.push(&wire).is_err());
        assert!(decoder.push(&hello).unwrap_err().0.contains("failed"));
    }
}

#[test]
fn rejects_truncated_and_oversized_framing() {
    let mut truncated = ServerMessageDecoder::default();
    assert!(truncated.push(&[0, 0, 0, 2, 1]).unwrap().is_empty());
    assert!(truncated.end().unwrap_err().0.to_lowercase().contains("truncated"));
    let mut oversized = ClientMessageDecoder::new(FrameDecoderOptions::with_max_frame_length(3));
    assert!(oversized.push(&[0, 0, 0, 4]).is_err());
}
