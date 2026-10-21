//! Port of upstream `test/flow.test.ts`: link flow control (windowed state, coalescing, overflow resets, relays,
//! peers without acks, bad acks) and the per-link call limit. Every case is ported.

mod chord2_support;

use std::sync::{Arc, Mutex};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::{Call, Frame, Hello, Hold, PROTOCOL_VERSION, Payload, PeerKind, StateFrame, StateKind};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, GetOptions, Host, HostOptions, Implementation, Link, LinkOptions,
    Readable, Received, RemoteDef, Route, ServeOptions, VariantDef, create_host, host_named, memory_link_pair, method,
    on_value, remote,
};
use pi_chord::{Json, json};

/// A client's end of a link that can hold back its acknowledgements, and records what it receives.
struct AckGate {
    inner: Arc<dyn Link>,
    received: Arc<Mutex<Vec<Frame>>>,
    /// Whether acks are held back, and the latest one held.
    gate: Mutex<(bool, Option<Frame>)>,
}

impl AckGate {
    fn new(inner: Arc<dyn Link>) -> Arc<Self> {
        Arc::new(Self { inner, received: Arc::default(), gate: Mutex::default() })
    }

    fn pause(&self) {
        self.gate.lock().unwrap().0 = true;
    }

    /// Lets the held acknowledgement through and stops holding them (under the lock: acks stay in order).
    fn release(&self) {
        let mut gate = self.gate.lock().unwrap();
        gate.0 = false;
        if let Some(held) = gate.1.take() {
            self.inner.send(&held);
        }
    }

    fn received(&self) -> Vec<Frame> {
        self.received.lock().unwrap().clone()
    }
}

impl Link for AckGate {
    fn closed(&self) -> bool {
        self.inner.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        if let Frame::Ack { .. } = frame {
            let mut gate = self.gate.lock().unwrap();
            if gate.0 {
                gate.1 = Some(frame.clone());
                return Some(0);
            }
            return self.inner.send(frame);
        }
        self.inner.send(frame)
    }

    fn receive(&self, handler: FrameHandler) {
        let received = self.received.clone();
        self.inner.receive(Arc::new(move |message| {
            if let Received::Frame(frame) = &message {
                received.lock().unwrap().push(frame.clone());
            }
            handler(message)
        }));
    }

    fn on_close(&self, handler: CloseHandler) {
        self.inner.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        self.inner.close(reason);
    }
}

/// A server-side link that records what the server sends to the worker.
struct Recorder {
    inner: Arc<dyn Link>,
    sent: Arc<Mutex<Vec<Frame>>>,
}

impl Link for Recorder {
    fn closed(&self) -> bool {
        self.inner.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        self.sent.lock().unwrap().push(frame.clone());
        self.inner.send(frame)
    }

    fn receive(&self, handler: FrameHandler) {
        self.inner.receive(handler);
    }

    fn on_close(&self, handler: CloseHandler) {
        self.inner.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        self.inner.close(reason);
    }
}

/// A counter on the server (`t.flow-counter`), or on the worker host `w` (`t.flow-worker-counter`).
fn counter_on(name: &str, host: &str) -> Contract {
    let main = VariantDef::new()
        .state("count", object(&[req("n", number())]))
        .method("add", method(number(), number()))
        .method("wait", method(None, None));
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named(host).unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote(name, &def).unwrap()
}

fn flow_counter() -> Contract {
    counter_on("t.flow-counter", "server")
}

fn worker_counter() -> Contract {
    counter_on("t.flow-worker-counter", "w")
}

/// What `wait` waits for (upstream `release`), and a log of the waits that started.
#[derive(Clone, Default)]
struct Waits {
    latch: Latch,
    entered: Log,
}

fn serve_flow_counter(host: &Host, contract: &Contract, waits: &Waits) {
    let waits = waits.clone();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let count = decl.state("count", object(&[req("n", number())]), json!({"n": 0}))?;
        let (adder, waits) = (count.clone(), waits.clone());
        Ok(Implementation::variant("main")
            .state("count", count)
            .method("add", move |delta, ctx| {
                let count = adder.clone();
                async move {
                    let delta = delta.as_f64().unwrap_or_default();
                    count.change(&ctx, |draft| {
                        let n = draft["n"].as_f64().unwrap_or_default();
                        draft.set("n", n + delta)
                    })?;
                    Ok(count.value()?["n"].clone())
                }
            })
            .method("wait", move |_, _| {
                let waits = waits.clone();
                async move {
                    waits.entered.push("wait");
                    waits.latch.wait().await;
                    Ok(Json::Null)
                }
            }))
    };
    host.serve(contract, factory, ServeOptions::default()).unwrap();
}

fn client_host(link: Arc<dyn Link>) -> Host {
    let upstream = Some((link, ConnectOptions::new(json!({"kind": "server"}))));
    create_host(HostOptions { kind: PeerKind::Client, upstream, ..host_options("browser", &Log::default()) })
}

fn windowed(window: usize) -> AcceptOptions {
    let link = LinkOptions { window: Some(window), ..LinkOptions::default() };
    AcceptOptions { link, ..AcceptOptions::client(json!({"user": "u"})) }
}

/// A client of `server` (accepted with `window`) whose acknowledgements can be held back.
fn client(server: &Host, window: usize) -> (Host, Arc<AckGate>) {
    let (down, up) = memory_link_pair();
    server.accept(down, windowed(window)).unwrap();
    let gate = AckGate::new(up);
    (client_host(gate.clone()), gate)
}

fn state_frames(frames: &[Frame]) -> Vec<StateFrame> {
    let state =
        frames.iter().filter_map(|frame| if let Frame::State(state) = frame { Some(state.clone()) } else { None });
    state.collect()
}

fn count_tag(frames: &[Frame], tag: &str) -> usize {
    frames.iter().filter(|frame| frame.tag() == tag).count()
}

/// Polls `check` every 2 ms for up to about 5 s (upstream `until` with sleeps).
async fn settle(check: impl Fn() -> bool) {
    for _ in 0..2500 {
        if check() {
            return;
        }
        sleep_ms(2).await;
    }
    assert!(check(), "condition not reached");
}

fn last_n(values: &Log<Json>) -> Option<f64> {
    values.all().last().and_then(|value| value["n"].as_f64())
}

#[tokio::test]
async fn holds_state_back_while_the_window_is_full_answers_calls_and_sends_one_coalesced_frame() {
    let _serial = serial().await;
    let server = named("server");
    serve_flow_counter(&server, &flow_counter(), &Waits::default());
    let (host, gate) = client(&server, 200);
    let counter = host.hold(&flow_counter(), json!({}), GetOptions::default()).unwrap();
    let values: Log<Json> = Log::default();
    let sink = values.clone();
    let _values = on_value(counter.state("count").unwrap().as_ref(), move |value| sink.push(value.clone()));
    settle(|| !values.is_empty()).await;

    gate.pause();
    let mut results = Vec::new();
    for _ in 0..40 {
        results.push(call(&counter, "add", json!(1)).await.unwrap());
    }
    // Every call was answered, though most state is still waiting for an acknowledgement.
    assert_eq!(results.last(), Some(&json!(40)));
    assert!(last_n(&values).unwrap() < 40.0, "state waits behind the window");
    let before = state_frames(&gate.received()).len();

    gate.release();
    settle(|| last_n(&values) == Some(40.0)).await;
    assert_eq!(state_frames(&gate.received()).len() - before, 1, "the waiting ops arrive as one frame");
    host.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn replaces_ops_that_outgrow_their_slot_with_the_current_value() {
    let _serial = serial().await;
    let server = named("server");
    serve_flow_counter(&server, &flow_counter(), &Waits::default());
    let (host, gate) = client(&server, 64);
    let counter = host.hold(&flow_counter(), json!({}), GetOptions::default()).unwrap();
    let values: Log<Json> = Log::default();
    let sink = values.clone();
    let _values = on_value(counter.state("count").unwrap().as_ref(), move |value| sink.push(value.clone()));
    settle(|| !values.is_empty()).await;

    gate.pause();
    let before = gate.received().len();
    for _ in 0..100 {
        call(&counter, "add", json!(1)).await.unwrap();
    }
    gate.release();
    settle(|| last_n(&values) == Some(100.0)).await;
    let sent = state_frames(&gate.received()[before..]);
    assert!(sent.iter().any(|frame| frame.kind == StateKind::Reset), "a reset replaced the ops");
    assert!(sent.len() < 10, "few state frames, not one per change ({})", sent.len());
    host.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn drops_what_waits_for_a_reference_that_is_released() {
    let _serial = serial().await;
    let server = named("server");
    serve_flow_counter(&server, &flow_counter(), &Waits::default());
    let (host, gate) = client(&server, 64);
    let counter = host.hold(&flow_counter(), json!({}), GetOptions::default()).unwrap();
    counter.ready().await.unwrap();
    let other = host.hold(&flow_counter(), json!({}), GetOptions::default()).unwrap();
    other.ready().await.unwrap();

    gate.pause();
    for _ in 0..10 {
        call(&counter, "add", json!(1)).await.unwrap();
    }
    counter.release();
    other.release();
    // The link task sends the release frame: once it answers, the frame is on the link ahead of the held ack.
    host.connections().await;
    let before = gate.received().len();
    gate.release();
    sleep_ms(30).await;
    assert_eq!(state_frames(&gate.received()[before..]), Vec::<StateFrame>::new());
    host.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn coalesces_relayed_state_for_the_origin_and_asks_downstream_for_a_reset_when_it_overflows() {
    let _serial = serial().await;
    let server = named("server");
    let to_worker: Arc<Mutex<Vec<Frame>>> = Arc::default();
    let sent = to_worker.clone();
    let resolve: pi_chord::host2::Resolve = Arc::new(move |name: &str| {
        let (down, up) = memory_link_pair();
        let worker = child_on(name, &up, LinkOptions::default(), &Log::default());
        serve_flow_counter(&worker, &worker_counter(), &Waits::default());
        let link: Arc<dyn Link> = Arc::new(Recorder { inner: down, sent: sent.clone() });
        Box::pin(std::future::ready(Ok(link)))
    });
    let authorize: pi_chord::host2::Authorize = Arc::new(|_: &Json, _: &str| Box::pin(std::future::ready(true)));
    let principal = Arc::new(|name: &str| json!({"host": name}));
    server.route("w", Route { authorize, resolve, principal });
    let (host, gate) = client(&server, 64);
    let counter = host.hold(&worker_counter(), json!({}), GetOptions::default()).unwrap();
    let values: Log<Json> = Log::default();
    let sink = values.clone();
    let _values = on_value(counter.state("count").unwrap().as_ref(), move |value| sink.push(value.clone()));
    settle(|| !values.is_empty()).await;
    let resyncs = || count_tag(&to_worker.lock().unwrap(), "resync");
    assert_eq!(resyncs(), 0);

    gate.pause();
    for _ in 0..100 {
        call(&counter, "add", json!(1)).await.unwrap();
    }
    // An overflow drops the waiting ops and asks the worker for a reset (which then waits in the slot).
    assert!(resyncs() >= 1, "the overflowing stream was resynced downstream");
    let frames = to_worker.lock().unwrap().clone();
    assert!(!frames.iter().any(|frame| matches!(frame, Frame::Sub { .. } | Frame::Unsub { .. })));
    gate.release();
    settle(|| last_n(&values) == Some(100.0)).await;
    host.close().await.unwrap();
    server.close().await.unwrap();
}

fn hello(acks: Option<bool>) -> Frame {
    let name = "raw".to_string();
    Frame::Hello(Hello { protocol: Some(PROTOCOL_VERSION), name, kind: PeerKind::Client, code_version: 0, acks })
}

#[tokio::test]
async fn neither_windows_nor_acknowledges_a_peer_whose_hello_does_not_announce_acks() {
    let _serial = serial().await;
    let server = named("server");
    let contract = flow_counter();
    serve_flow_counter(&server, &contract, &Waits::default());
    let (down, raw) = memory_link_pair();
    server.accept(down, windowed(1)).unwrap();
    let frames: Log<Frame> = Log::default();
    let sink = frames.clone();
    raw.receive(Arc::new(move |message| {
        if let Received::Frame(frame) = message {
            sink.push(frame);
        }
    }));
    raw.send(&hello(None));
    let token = contract.description().clone();
    let input = json!({});
    let hold =
        Hold { id: 1, at: "server".into(), token, input, principal: None, origin: None, state: true, again: false };
    raw.send(&Frame::Hold(Box::new(hold)));
    for i in 0..5 {
        let args = Some(Payload::Value(json!(1)));
        raw.send(&Frame::Call(Call { id: 3 + i, hold: 1, member: "add".into(), args, root: None }));
    }
    settle(|| count_tag(&frames.all(), "result") == 5).await;
    sleep_ms(20).await;
    let frames = frames.all();
    assert_eq!(state_frames(&frames).len(), 6, "the snapshot and one frame per change");
    assert!(!frames.iter().any(|frame| matches!(frame, Frame::Ack { .. })));
    let Frame::Hello(first) = &frames[0] else { panic!("first frame: {:?}", frames[0]) };
    assert_eq!(first.acks, Some(true));
    server.close().await.unwrap();
}

#[tokio::test]
async fn closes_the_link_on_an_acknowledgement_of_frames_never_sent() {
    let _serial = serial().await;
    let server = named("server");
    let (down, raw) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"user": "u"}))).unwrap();
    raw.receive(Arc::new(|_| {}));
    let reasons: Log = Log::default();
    let sink = reasons.clone();
    raw.on_close(Box::new(move |reason| sink.push(reason)));
    // Upstream's hello has no protocol (so a version mismatch closes the link); this one closes on the ack.
    raw.send(&hello(Some(true)));
    raw.send(&Frame::Ack { n: 99 });
    settle(|| raw.closed()).await;
    assert!(reasons.all()[0].contains("ack 99"), "{:?}", reasons.all());
    server.close().await.unwrap();
}

#[tokio::test]
async fn answers_calls_beyond_max_calls_at_once_with_too_many_calls_and_keeps_the_link() {
    let _serial = serial().await;
    let server = named("server");
    let waits = Waits::default();
    serve_flow_counter(&server, &flow_counter(), &waits);
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions { max_calls: Some(2), ..AcceptOptions::client(json!({"user": "u"})) }).unwrap();
    let host = client_host(up);
    let counter = host.hold(&flow_counter(), json!({}), GetOptions::default()).unwrap();
    counter.ready().await.unwrap();
    let wait = |counter: pi_chord::host2::Ref| tokio::spawn(async move { counter.call("wait", None, None).await });
    let (first, second) = (wait(counter.clone()), wait(counter.clone()));
    settle(|| waits.entered.len() == 2).await;
    let error = call(&counter, "add", json!(1)).await.unwrap_err();
    assert!(is(&error, "unavailable") && error.reason() == Some("too_many_calls"), "{error}");
    waits.latch.open(json!(true));
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(call(&counter, "add", json!(1)).await.unwrap(), json!(1));
    host.close().await.unwrap();
    server.close().await.unwrap();
}
