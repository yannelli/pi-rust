//! Memory links (upstream `link.ts`): ordering, `bye`, close handlers, severing, and the packed-payload tests of
//! upstream `test/link.test.ts` ("only payloads above the threshold are packed", the relay forwarding bytes).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::host2::link::{Link, MemoryLink, PACK_THRESHOLD, Received, memory_link_pair, unpack};
use pi_chord::host2::wire::{CallResult, Frame, Payload, StateFrame, StateKind};
use pi_chord::{Json, json};

/// About 160,000 characters of JSON: above `PACK_THRESHOLD`.
fn large(tag: &str) -> Json {
    Json::array((0..40).map(|index| Json::from(format!("{tag}{index}:{}", "x".repeat(4000)))).collect())
}

fn result(id: u64, value: Json) -> Frame {
    Frame::Result(CallResult { id, value: Some(Payload::Value(value)), error: None })
}

fn recorder(link: &MemoryLink) -> Arc<Mutex<Vec<Received>>> {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    link.receive(Arc::new(move |frame| sink.lock().unwrap().push(frame)));
    received
}

async fn until(condition: impl Fn() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("condition not met");
}

fn frames_of(received: &Mutex<Vec<Received>>) -> Vec<Frame> {
    let received = received.lock().unwrap();
    received
        .iter()
        .map(|item| if let Received::Frame(frame) = item { frame.clone() } else { panic!("{item:?}") })
        .collect()
}

fn payload(frame: &Frame) -> &Payload {
    match frame {
        Frame::Result(CallResult { value: Some(payload), .. })
        | Frame::State(StateFrame { value: Some(payload), .. }) => payload,
        other => panic!("no payload: {other:?}"),
    }
}

#[tokio::test]
async fn only_payloads_above_the_threshold_are_packed_and_the_receiver_unpacks_them() {
    let (a, b) = memory_link_pair();
    let received = recorder(&b);
    a.send(&result(1, large("v")));
    a.send(&result(2, json!(["small"])));
    let snap = StateFrame {
        hold: 3,
        m: "notes".into(),
        kind: StateKind::Snap,
        value: Some(Payload::Value(large("s"))),
        ops: None,
        error: None,
    };
    a.send(&Frame::State(snap));
    until(|| received.lock().unwrap().len() == 3).await;
    let frames = frames_of(&received);
    let Payload::Packed(packed) = payload(&frames[0]) else { panic!("not packed") };
    assert_eq!(unpack(payload(&frames[0])).unwrap(), large("v"));
    assert_eq!(payload(&frames[1]), &Payload::Value(json!(["small"])));
    assert!(matches!(payload(&frames[2]), Payload::Packed(_)));
    assert_eq!(unpack(payload(&frames[2])).unwrap(), large("s"));

    // A relay forwards the packed bytes as they are.
    let (c, d) = memory_link_pair();
    let forwarded = recorder(&d);
    let Frame::Result(first) = &frames[0] else { panic!() };
    c.send(&Frame::Result(CallResult { id: 9, ..first.clone() }));
    until(|| forwarded.lock().unwrap().len() == 1).await;
    let relayed = frames_of(&forwarded);
    let Payload::Packed(relayed) = payload(&relayed[0]) else { panic!("not packed") };
    assert_eq!(relayed.bytes(), packed.bytes());
}

#[tokio::test]
async fn send_reports_the_utf16_length_of_the_message() {
    let (a, _b) = memory_link_pair();
    let text = r#"{"t":"result","id":1,"value":"é😀"}"#;
    assert_eq!(a.send(&result(1, json!("é😀"))), Some(text.encode_utf16().count()));
    let packed = a.send(&result(2, Json::from("x".repeat(PACK_THRESHOLD)))).unwrap();
    assert!(packed < 1000, "a packed payload travels deflated: {packed}");
}

#[tokio::test]
async fn frames_before_the_receiver_are_queued_in_order() {
    let (a, b) = memory_link_pair();
    for n in 1..=3 {
        a.send(&Frame::Ping { n });
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    let received = recorder(&b);
    a.send(&Frame::Ping { n: 4 });
    until(|| received.lock().unwrap().len() == 4).await;
    let numbers: Vec<_> =
        frames_of(&received).iter().map(|frame| if let Frame::Ping { n } = frame { *n } else { 0 }).collect();
    assert_eq!(numbers, [1, 2, 3, 4]);
}

#[tokio::test]
async fn close_sends_bye_and_notifies_both_ends_once() {
    let (a, b) = memory_link_pair();
    let received = recorder(&b);
    let reasons = Arc::new(Mutex::new(Vec::new()));
    for (name, link) in [("a", &a), ("b", &b)] {
        let reasons = reasons.clone();
        link.on_close(Box::new(move |reason| reasons.lock().unwrap().push(format!("{name}: {reason}"))));
    }
    a.send(&Frame::Ping { n: 1 });
    a.close(Some("done"));
    assert!(a.closed());
    assert_eq!(a.send(&Frame::Ping { n: 2 }), None);
    until(|| b.closed()).await;
    // The frame sent before the close arrives first; the `bye` itself is consumed by the link.
    assert_eq!(frames_of(&received), [Frame::Ping { n: 1 }]);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*reasons.lock().unwrap(), ["a: done", "b: done"]);
    let late = Arc::new(Mutex::new(None));
    let sink = late.clone();
    b.on_close(Box::new(move |reason| *sink.lock().unwrap() = Some(reason.to_string())));
    assert_eq!(late.lock().unwrap().as_deref(), Some("closed"));
}

#[tokio::test]
async fn a_peer_without_a_receiver_still_ends_on_bye() {
    let (a, b) = memory_link_pair();
    a.send(&Frame::Ping { n: 1 });
    a.send(&Frame::Bye { reason: None });
    until(|| b.closed()).await;
    // Frames queued for a receiver that never came are dropped with the link.
    let received = recorder(&b);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_severed_link_drops_frames_silently() {
    let (a, b) = memory_link_pair();
    let received = recorder(&b);
    a.sever();
    assert_eq!(a.send(&Frame::Ping { n: 1 }), None);
    assert_eq!(b.send(&Frame::Ping { n: 2 }), None);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(received.lock().unwrap().is_empty());
    assert!(!a.closed() && !b.closed());
}

#[tokio::test]
async fn malformed_frames_reach_the_receiver_as_such() {
    let (a, b) = memory_link_pair();
    let received = recorder(&b);
    // A shape whose state names an empty member: a valid frame for the sender's types, not for `isFrame`.
    let shape = pi_chord::host2::wire::Shape {
        id: 1,
        variant: None,
        members: Vec::new(),
        contract: "x".into(),
        state: vec![String::new()],
    };
    a.send(&Frame::Shape(shape));
    until(|| received.lock().unwrap().len() == 1).await;
    let expected = json!({"t": "shape", "id": 1, "members": [], "contract": "x", "state": [""]});
    assert_eq!(received.lock().unwrap()[0], Received::Malformed(expected));
}

#[tokio::test]
async fn a_receiver_may_send_and_close_from_inside_its_handler() {
    let (a, b) = memory_link_pair();
    let b_send = b.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    b.receive(Arc::new(move |received| {
        if let Received::Frame(Frame::Ping { n }) = received {
            sink.lock().unwrap().push(n);
            b_send.send(&Frame::Pong { n });
            if n == 2 {
                b_send.close(None);
            }
        }
    }));
    let pongs = recorder(&a);
    a.send(&Frame::Ping { n: 1 });
    a.send(&Frame::Ping { n: 2 });
    a.send(&Frame::Ping { n: 3 });
    until(|| a.closed()).await;
    assert_eq!(*seen.lock().unwrap(), [1, 2]);
    assert_eq!(frames_of(&pongs), [Frame::Pong { n: 1 }, Frame::Pong { n: 2 }]);
}
