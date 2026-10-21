//! Wire frames (upstream `wire.ts`) and payload packing (`link.ts`) against golden fixtures from upstream
//! TypeScript: each frame as the IPC/memory object and as WebSocket messages, packing around the threshold, frame
//! validation verdicts and wire errors.

mod chord2_support;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chord2_support::*;
use pi_chord::host2::link::{
    Message, PACK_THRESHOLD, Received, inflate_message, pack_message, pack_object, parse_frame, unpack, unpack_message,
};
use pi_chord::host2::wire::{Call, CallResult, Frame, PROTOCOL_VERSION, Payload, RootRef, StateFrame, StateKind};
use pi_chord::host2::{ChordErrorCode, from_wire_error, is_frame, to_wire_error};
use pi_chord::{ChordError, Json, json};

fn frame_of(text: &str) -> Frame {
    match parse_frame(text).unwrap() {
        Received::Frame(frame) => frame,
        Received::Malformed(value) => panic!("malformed: {value}"),
    }
}

fn assert_message(message: &Message, expected: &Json, label: &str) {
    match (message, expected.get("text")) {
        (Message::Text(actual), Some(text)) => assert_digest(actual, text, label),
        (Message::Binary(actual), None) => assert_eq!(STANDARD.encode(actual), text(&expected["binary"]), "{label}"),
        _ => panic!("{label}: expected {expected}"),
    }
}

#[test]
fn frames_are_written_like_upstream() {
    let fixture = fixture("frames.json");
    assert_eq!(fixture["protocol"].as_u64(), Some(PROTOCOL_VERSION));
    assert_eq!(fixture["packThreshold"].as_u64(), Some(PACK_THRESHOLD as u64));
    for entry in fixture["frames"].as_array().unwrap() {
        let label = text(&entry["label"]);
        let object = text(&entry["object"]);
        assert_eq!(is_frame(&Json::parse(object).unwrap()), entry["valid"].as_bool().unwrap(), "{label}");
        let frame = frame_of(object);
        assert_eq!(pack_object(&frame, true).to_string(), object, "{label}: object");
        let ws = pack_message(&frame, true);
        assert_message(&ws, &entry["ws"], label);
        assert_eq!(ws.size() as f64, entry["wsSize"].as_f64().unwrap(), "{label}: size");
        assert_message(&pack_message(&frame, false), &entry["wsPlain"], label);
    }
}

fn large() -> Json {
    let items = (0..2000).map(|item| json!({"item": item, "text": format!("line {item} \u{e9}\u{1f600}")}));
    Json::array(items.collect())
}

/// The frames of `packed.json`, built here.
fn packed_frame(label: &str) -> Frame {
    let result = |id, value: Json| Frame::Result(CallResult { id, value: Some(Payload::Value(value)), error: None });
    let state = |kind, value: Json| {
        let value = Some(Payload::Value(value));
        Frame::State(StateFrame { hold: 4, m: "notes".into(), kind, value, ops: None, error: None })
    };
    match label {
        "atThreshold" => result(1, Json::from("x".repeat(PACK_THRESHOLD - 2))),
        "overThreshold" => result(2, Json::from("x".repeat(PACK_THRESHOLD - 1))),
        "multibyteUnder" => result(3, Json::from("\u{e9}".repeat(40_000))),
        "astralOver" => state(StateKind::Snap, Json::from("\u{1f600}".repeat(33_000))),
        "largeArgs" => Frame::Call(Call {
            id: 5,
            hold: 1,
            member: "echo".into(),
            args: Some(Payload::Value(large())),
            root: Some(RootRef { root_id: "r".into(), code_version: 1 }),
        }),
        "largeReset" => state(StateKind::Reset, large()),
        other => panic!("unknown frame {other}"),
    }
}

fn payload_of(frame: &Frame) -> &Payload {
    match frame {
        Frame::Call(Call { args: Some(payload), .. })
        | Frame::Result(CallResult { value: Some(payload), .. })
        | Frame::State(StateFrame { value: Some(payload), .. }) => payload,
        other => panic!("no payload: {other:?}"),
    }
}

fn packed_bytes(frame: &Frame) -> &[u8] {
    match payload_of(frame) {
        Payload::Packed(packed) => packed.bytes(),
        Payload::Value(_) => panic!("not packed: {frame:?}"),
    }
}

/// A binary message's envelope: the length prefix and the envelope JSON.
fn head(message: &[u8]) -> &[u8] {
    let length = u32::from_be_bytes(message[..4].try_into().unwrap()) as usize;
    &message[..4 + length]
}

#[test]
fn payloads_above_the_threshold_are_packed_like_upstream() {
    let fixture = fixture("packed.json");
    for entry in fixture["frames"].as_array().unwrap() {
        let label = text(&entry["label"]);
        let frame = packed_frame(label);
        let value = unpack(payload_of(&frame)).unwrap();
        assert_digest(&value.to_string(), &entry["payload"], label);
        assert_message(&pack_message(&frame, false), &entry["wsPlain"], label);
        let Some(node) = entry["ws"].get("binary") else {
            // Not packed: the text frames match byte for byte.
            let ws = pack_message(&frame, true);
            assert_message(&ws, &entry["ws"], label);
            assert_eq!(ws.size() as f64, entry["wsSize"].as_f64().unwrap(), "{label}: size");
            assert_digest(&pack_object(&frame, true).to_string(), &entry["object"], label);
            continue;
        };
        // Node's deflated bytes: Rust inflates them, and a relay forwards them unchanged.
        let node = STANDARD.decode(text(node)).unwrap();
        assert_digest(&inflate_message(&node).unwrap(), &entry["ws"]["inflated"], label);
        let Received::Frame(received) = unpack_message(&node).unwrap() else { panic!("{label}: malformed") };
        assert_eq!(unpack(payload_of(&received)).unwrap(), value, "{label}: unpacked");
        let relayed = pack_message(&received, false);
        assert_eq!(relayed, Message::Binary(node.clone()), "{label}: relayed");
        assert_eq!(relayed.size() as f64, entry["wsSize"].as_f64().unwrap(), "{label}: size");
        let object = text(&entry["object"]);
        let from_object = frame_of(object);
        assert_eq!(pack_object(&from_object, false).to_string(), object, "{label}: object relayed");
        assert_eq!(unpack(payload_of(&from_object)).unwrap(), value, "{label}: object unpacked");

        // Rust's own deflate: the same envelope, and bytes that inflate to the same payload.
        let Message::Binary(ours) = pack_message(&frame, true) else { panic!("{label}: not packed") };
        assert_eq!(head(&ours), head(&node), "{label}: envelope");
        assert_digest(&inflate_message(&ours).unwrap(), &entry["ws"]["inflated"], label);
        let ours = pack_object(&frame, true);
        let theirs = Json::parse(object).unwrap();
        assert_eq!(ours["z"][0], theirs["z"][0], "{label}: z field");
        let mut envelope = ours.as_object().unwrap().clone();
        envelope.remove("z");
        let mut expected = theirs.as_object().unwrap().clone();
        expected.remove("z");
        assert_eq!(Json::object(envelope).to_string(), Json::object(expected).to_string(), "{label}: envelope");
        let Received::Frame(back) = pack_object_round_trip(&ours) else { panic!("{label}: malformed") };
        assert!(!packed_bytes(&back).is_empty());
        assert_eq!(unpack(payload_of(&back)).unwrap(), value, "{label}: own bytes");
    }
}

fn pack_object_round_trip(object: &Json) -> Received {
    parse_frame(&object.to_string()).unwrap()
}

#[test]
fn frame_validation_matches_upstream_is_frame() {
    for case in fixture("is_frame.json").as_array().unwrap() {
        assert_eq!(is_frame(&case["value"]), case["valid"].as_bool().unwrap(), "{}", case["value"]);
    }
}

#[test]
fn wire_errors_round_trip_like_upstream() {
    for case in fixture("wire_errors.json").as_array().unwrap() {
        let sent = &case["sent"];
        let frame = json!({"t": "gone", "id": 1, "error": sent.to_serde()});
        let Some(Frame::Gone { error, .. }) = Frame::from_json(&frame) else { panic!("invalid: {sent}") };
        assert_eq!(error.to_json(), *sent);
        let back = from_wire_error(&error);
        let actual = json!({
            "code": back.code().unwrap().as_str(),
            "reason": back.reason(),
            "message": back.to_string(),
            "retryable": back.retryable(),
        });
        assert_eq!(actual, case["back"], "{sent}");
        // A non-chord error gains its prefix on the way back; every other error round-trips unchanged.
        if text(&sent["message"]) == back.to_string() {
            assert_eq!(to_wire_error(&back), error, "{sent}");
        }
    }
    let plain = to_wire_error(&ChordError::error("plain error"));
    assert_eq!(plain.to_json(), json!({"code": "unavailable", "message": "plain error"}));
    let superseded = from_wire_error(&to_wire_error(&ChordError::code_only(ChordErrorCode::Superseded, "x")));
    assert!(superseded.retryable());
    assert_eq!(superseded.to_string(), "superseded: x");
}

#[test]
fn unknown_fields_are_accepted_and_tokens_keep_theirs() {
    let hello = json!({"t": "hello", "name": "x", "kind": "host", "codeVersion": 1, "extra": {"anything": true}});
    let frame = Frame::from_json(&hello).unwrap();
    assert_eq!(pack_object(&frame, true).to_string(), r#"{"t":"hello","name":"x","kind":"host","codeVersion":1}"#);
    let token = json!({"name": "t.x", "scope": "local", "version": 1, "input": {}, "variants": {"": {}}, "later": 1});
    let hold = json!({"t": "hold", "id": 3, "at": "server", "token": token.to_serde(), "input": null});
    let frame = Frame::from_json(&hold).unwrap();
    let written = pack_object(&frame, true);
    assert_eq!(written["token"], token);
    assert_eq!(written.to_string(), hold.to_string());
}

#[test]
fn messages_that_are_not_frames_are_protocol_errors() {
    for bad in ["[]", "null", "{}", r#"{"t":5}"#, "not json", r#"{"t":"result","id":1,"z":["other","AA=="]}"#] {
        assert!(parse_frame(bad).is_err(), "{bad}");
    }
    let malformed = parse_frame(r#"{"t":"ack","n":0}"#).unwrap();
    assert_eq!(malformed, Received::Malformed(json!({"t": "ack", "n": 0})));
    assert!(unpack_message(&[0, 0]).is_err());
    assert!(unpack_message(b"\0\0\0\x02{}").is_err());
}
