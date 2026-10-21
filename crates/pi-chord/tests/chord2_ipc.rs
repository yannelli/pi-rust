//! IPC links (upstream `link.ts` and "links: IPC" in `test/link.test.ts`): frames travel as `{ chord2: frame }`
//! messages, and [`StreamChannel`] writes each as one line of JSON like Node's IPC, checked against golden frames.

mod chord2_support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chord2_support::{fixture, text};
use pi_chord::host2::link::{
    DisconnectListener, IpcChannel, IpcSendError, ListenerId, MessageListener, PACK_THRESHOLD, Packed, Received,
    SendCallback, StreamChannel, ipc_link, parse_frame,
};
use pi_chord::host2::wire::{CallResult, Frame, Payload};
use pi_chord::host2::{IpcLink, Link};
use pi_chord::{Json, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

async fn until(condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("condition not met");
}

fn frame_of(text: &str) -> Frame {
    match parse_frame(text).unwrap() {
        Received::Frame(frame) => frame,
        Received::Malformed(value) => panic!("malformed: {value}"),
    }
}

/// A channel whose every write fails a moment later, as Node reports `EPIPE` after the peer process exited.
#[derive(Default)]
struct Broken {
    sent: Mutex<Vec<Json>>,
    removed: Mutex<Vec<ListenerId>>,
    next: AtomicU64,
}

impl IpcChannel for Broken {
    fn connected(&self) -> bool {
        true
    }

    fn send(&self, message: Json, done: Option<SendCallback>) -> bool {
        self.sent.lock().unwrap().push(message);
        let done = done.expect("an IPC send without a callback cannot report EPIPE");
        tokio::spawn(async move { done(Err(IpcSendError::new(Some("EPIPE"), "write EPIPE"))) });
        false
    }

    fn on_message(&self, _listener: MessageListener) -> ListenerId {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    fn on_disconnect(&self, _listener: DisconnectListener) -> ListenerId {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    fn off(&self, id: ListenerId) {
        self.removed.lock().unwrap().push(id);
    }
}

fn close_reasons(link: &IpcLink) -> Arc<Mutex<Vec<String>>> {
    let reasons = Arc::new(Mutex::new(Vec::new()));
    let sink = reasons.clone();
    link.on_close(Box::new(move |reason| sink.lock().unwrap().push(reason.to_string())));
    reasons
}

fn recorder(link: &IpcLink) -> Arc<Mutex<Vec<Received>>> {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    link.receive(Arc::new(move |item| sink.lock().unwrap().push(item)));
    received
}

#[tokio::test]
async fn a_write_that_fails_after_the_peer_exited_ends_the_link() {
    let channel = Arc::new(Broken::default());
    let link = ipc_link(channel.clone());
    let reasons = close_reasons(&link);
    link.send(&Frame::Ping { n: 1 });
    until(|| link.closed()).await;
    assert_eq!(*reasons.lock().unwrap(), ["ipc send failed: EPIPE"]);
    assert_eq!(channel.sent.lock().unwrap().len(), 1);
    assert_eq!(channel.removed.lock().unwrap().len(), 2, "the link stops listening");
    link.send(&Frame::Ping { n: 2 });
    assert_eq!(channel.sent.lock().unwrap().len(), 1, "a closed link sends nothing more");
}

type Raw = (BufReader<ReadHalf<DuplexStream>>, WriteHalf<DuplexStream>);

/// A stream channel and the raw other end of its stream.
fn stream_pair() -> (Arc<StreamChannel>, Raw) {
    let (ours, theirs) = tokio::io::duplex(1 << 22);
    let channel = StreamChannel::new(async move { Ok(ours) });
    let (read, write) = tokio::io::split(theirs);
    (channel, (BufReader::new(read), write))
}

async fn read_line(raw: &mut Raw) -> String {
    let mut line = String::new();
    raw.0.read_line(&mut line).await.unwrap();
    line
}

#[tokio::test]
async fn frames_go_out_as_node_ipc_lines() {
    let (channel, mut raw) = stream_pair();
    let link = ipc_link(channel);
    let mut objects = Vec::new();
    for entry in fixture("frames.json")["frames"].as_array().unwrap() {
        if entry["valid"].as_bool() == Some(true) {
            objects.push(text(&entry["object"]).to_string());
        }
    }
    // Payloads Node packed: a relay forwards the bytes as they are, so the line matches byte for byte.
    for entry in fixture("packed.json")["frames"].as_array().unwrap() {
        if let Some(object) = entry["object"].as_str() {
            objects.push(object.to_string());
        }
    }
    for object in &objects {
        assert_eq!(link.send(&frame_of(object)), None, "IPC links are not windowed");
        assert_eq!(read_line(&mut raw).await, format!("{{\"chord2\":{object}}}\n"));
    }
}

#[tokio::test]
async fn node_ipc_lines_come_in_as_frames() {
    let (channel, mut raw) = stream_pair();
    let link = ipc_link(channel);
    let received = recorder(&link);
    let mut expected = Vec::new();
    let entries = fixture("frames.json")["frames"].as_array().unwrap().clone();
    let packed = fixture("packed.json")["frames"].as_array().unwrap().clone();
    let objects = entries.iter().filter(|entry| entry["valid"].as_bool() == Some(true)).map(|entry| &entry["object"]);
    for object in objects.chain(packed.iter().map(|entry| &entry["object"])).filter_map(Json::as_str) {
        let frame = frame_of(object);
        if matches!(frame, Frame::Bye { .. }) {
            continue;
        }
        raw.1.write_all(format!("{{\"chord2\":{object}}}\n").as_bytes()).await.unwrap();
        expected.push(Received::Frame(frame));
    }
    until(|| received.lock().unwrap().len() == expected.len()).await;
    assert_eq!(*received.lock().unwrap(), expected);
    assert!(!link.closed());
}

#[tokio::test]
async fn large_payloads_go_out_packed_with_padded_base64() {
    let (channel, mut raw) = stream_pair();
    let link = ipc_link(channel);
    let value = Json::from("y".repeat(PACK_THRESHOLD + 10));
    link.send(&Frame::Result(CallResult { id: 7, value: Some(Payload::Value(value.clone())), error: None }));
    let line = Json::parse(read_line(&mut raw).await.trim_end()).unwrap();
    let frame = &line["chord2"];
    assert_eq!((frame["t"].as_str(), frame["id"].as_u64()), (Some("result"), Some(7)));
    assert_eq!(frame["z"][0], json!("value"));
    let encoded = text(&frame["z"][1]);
    assert_eq!(encoded.len() % 4, 0, "padded base64");
    let packed = Packed::new(STANDARD.decode(encoded).unwrap());
    assert_eq!(Json::parse(&packed.inflate().unwrap()).unwrap(), value);
    assert!(frame.get("value").is_none());
}

#[tokio::test]
async fn control_messages_pass_by_the_link() {
    let (channel, mut raw) = stream_pair();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    channel.on_message(Arc::new(move |message| sink.lock().unwrap().push(message.clone())));
    let link = ipc_link(channel);
    let received = recorder(&link);
    raw.1
        .write_all(b"{\"chord2control\":{\"type\":\"ready\"}}\n{\"chord2\":{\"t\":\"ping\",\"n\":1}}\n")
        .await
        .unwrap();
    until(|| seen.lock().unwrap().len() == 2).await;
    assert_eq!(*received.lock().unwrap(), [Received::Frame(Frame::Ping { n: 1 })]);
    assert_eq!(seen.lock().unwrap()[0], json!({"chord2control": {"type": "ready"}}));
}

#[tokio::test]
async fn the_end_of_the_stream_disconnects_the_channel_and_ends_the_link() {
    let (channel, raw) = stream_pair();
    let link = ipc_link(channel.clone());
    let reasons = close_reasons(&link);
    drop(raw);
    until(|| link.closed()).await;
    assert_eq!(*reasons.lock().unwrap(), ["ipc disconnected"]);
    assert!(!channel.connected());
    let failed = Arc::new(Mutex::new(None));
    let sink = failed.clone();
    let done: SendCallback = Box::new(move |result| *sink.lock().unwrap() = result.err());
    assert!(!channel.send(json!({"late": true}), Some(done)));
    let code = failed.lock().unwrap().clone().and_then(|error| error.code);
    assert_eq!(code.as_deref(), Some("ERR_IPC_CHANNEL_CLOSED"));
}

#[tokio::test]
async fn close_sends_a_bye_and_a_bye_from_the_peer_ends_the_link() {
    let (channel, mut raw) = stream_pair();
    let link = ipc_link(channel);
    let reasons = close_reasons(&link);
    link.close(Some("done"));
    assert_eq!(read_line(&mut raw).await, "{\"chord2\":{\"t\":\"bye\",\"reason\":\"done\"}}\n");
    assert_eq!(*reasons.lock().unwrap(), ["done"]);

    let (channel, mut raw) = stream_pair();
    let link = ipc_link(channel);
    let reasons = close_reasons(&link);
    raw.1.write_all(b"{\"chord2\":{\"t\":\"bye\",\"reason\":\"later\"}}\n").await.unwrap();
    until(|| link.closed()).await;
    assert_eq!(*reasons.lock().unwrap(), ["later"]);
}

#[tokio::test]
async fn a_bad_packed_payload_is_a_protocol_error() {
    let (channel, mut raw) = stream_pair();
    let link = ipc_link(channel);
    let reasons = close_reasons(&link);
    raw.1.write_all(b"{\"chord2\":{\"t\":\"result\",\"id\":1,\"z\":[\"nope\",\"AA==\"]}}\n").await.unwrap();
    assert_eq!(read_line(&mut raw).await, "{\"chord2\":{\"t\":\"bye\",\"reason\":\"protocol error\"}}\n");
    assert_eq!(*reasons.lock().unwrap(), ["protocol error"]);
}

#[tokio::test]
async fn a_line_that_is_not_json_disconnects_the_channel() {
    let (channel, mut raw) = stream_pair();
    let link = ipc_link(channel.clone());
    let reasons = close_reasons(&link);
    raw.1.write_all(b"not json\n").await.unwrap();
    until(|| link.closed()).await;
    assert_eq!(*reasons.lock().unwrap(), ["ipc disconnected"]);
    assert!(!channel.connected());
}

#[tokio::test]
async fn messages_wait_for_the_stream_and_reading_waits_for_a_listener() {
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let (connect, connected) = tokio::sync::oneshot::channel();
    let channel = StreamChannel::new(async move { connected.await.map_err(std::io::Error::other) });
    for n in 1..=3 {
        assert!(channel.send(json!({"n": n}), None));
    }
    let flushed = channel.flushed();
    connect.send(ours).unwrap();
    flushed.await;
    let (read, mut write) = tokio::io::split(theirs);
    let mut lines = BufReader::new(read).lines();
    for n in 1..=3 {
        assert_eq!(lines.next_line().await.unwrap().unwrap(), format!("{{\"n\":{n}}}"));
    }
    // Sent before anyone listens: the first listener gets it.
    write.write_all(b"{\"early\":true}\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    channel.on_message(Arc::new(move |message| sink.lock().unwrap().push(message.clone())));
    until(|| !seen.lock().unwrap().is_empty()).await;
    assert_eq!(seen.lock().unwrap()[0], json!({"early": true}));
}
