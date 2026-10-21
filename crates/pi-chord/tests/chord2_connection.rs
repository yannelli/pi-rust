//! Port of upstream `test/link.test.ts`, connection cases: calls and state across hosts, methods without
//! arguments, protocol versions and packed payloads over a relay. Relays, stamping and lifetime are in
//! `chord2_connection_relay.rs`, the memory-link cases in `chord2_link.rs`, the IPC case in `chord2_ipc.rs`.

mod chord2_support;

use std::future::ready;
use std::sync::Arc;

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::{Frame, Hello, Hold, PROTOCOL_VERSION, PeerKind};
use pi_chord::host2::{
    AcceptOptions, Authorize, ConnectOptions, Contract, Declare, GetOptions, HostOptions, Implementation, Link,
    LinkOptions, MismatchHandler, ProtocolVersions, Received, RefStatus, RemoteDef, Resolve, RootOptions, Route,
    ServeOptions, VariantDef, create_host, host_named, memory_link_pair, method, on_event, on_value, remote,
};
use pi_chord::{Json, json};

#[tokio::test]
async fn a_child_calls_the_server_and_the_server_stamps_the_child_identity() {
    let _serial = serial().await;
    let pair = server_with_worker("session:S1");
    let principals: Log<Json> = Log::default();
    serve_counter(&pair.server, &principals);
    let result = pair
        .worker
        .root(
            |ctx| async move {
                let counter = ctx.get(&counter(), json!({"name": "c"}), GetOptions::default()).await?;
                assert_eq!(counter.variant().as_deref(), Some("main"));
                let added = counter.call_json("add", json!(2), Some(&ctx)).await?;
                let who = counter.call_json("who", Json::Null, Some(&ctx)).await?;
                Ok(json!([added, who]))
            },
            RootOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result, json!([2, {"worker": "session:S1"}]));
    assert_eq!(principals.all(), vec![json!({"worker": "session:S1"})]);
}

#[tokio::test]
async fn state_members_snapshot_then_ops_and_values_decode_to_the_provider_ones() {
    let _serial = serial().await;
    let pair = server_with_worker("session:S1");
    serve_counter(&pair.server, &Log::default());
    let counter = pair.worker.hold(&counter(), json!({"name": "s"}), GetOptions::default()).unwrap();
    let (events, values): (Log, Log<Json>) = (Log::default(), Log::default());
    let count = counter.state("count").unwrap();
    let (seen, kinds) = (values.clone(), events.clone());
    let _events = on_event(count.as_ref(), move |event| kinds.push(event.kind()));
    let _values = on_value(count.as_ref(), move |value| seen.push(value.clone()));
    counter.ready().await.unwrap();
    call(&counter, "add", json!(1)).await.unwrap();
    call(&counter, "add", json!(1)).await.unwrap();
    until(|| values.len() == 3).await;
    assert_eq!(count.value().unwrap(), json!({"n": 2}));
    // Listeners added before the hydration get the snapshot then (not a missing value first).
    assert_eq!(events.all(), vec!["snap", "ops", "ops"]);
    assert_eq!(values.all(), vec![json!({"n": 0}), json!({"n": 1}), json!({"n": 2})]);
}

#[tokio::test]
async fn the_server_reaches_a_connected_child_by_name() {
    let _serial = serial().await;
    let pair = server_with_worker("session:w1");
    let principals: Log<Json> = Log::default();
    serve_counter(&pair.worker, &principals);
    tick().await;
    let counter = pair.server.hold(&session_counter(), json!({"session": "w1", "name": "x"}), GetOptions::default());
    assert_eq!(call(&counter.unwrap(), "add", json!(5)).await.unwrap(), json!(5));
    // The parent's own requests use the identity the child gave `connect`.
    assert_eq!(principals.all(), vec![json!({"kind": "server"})]);
}

#[tokio::test]
async fn rejects_unknown_types_on_the_server() {
    let _serial = serial().await;
    let pair = server_with_worker("session:S1");
    let missing = pair.worker.hold(&counter(), json!({"name": "m"}), GetOptions::default()).unwrap();
    let error = missing.ready().await.unwrap_err();
    assert!(is(&error, "unavailable"), "{error}");
    assert_eq!(missing.status(), RefStatus::Gone);
}

/// A link that records the frames it sends.
struct Recording {
    inner: Memory,
    sent: Log<Frame>,
}

impl Link for Recording {
    fn closed(&self) -> bool {
        self.inner.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        self.sent.push(frame.clone());
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

fn tool() -> Contract {
    let main = VariantDef::new()
        .method("ping", method(None, None))
        .method("count", method(None, number()))
        .method("note", method(string(), None));
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named("server").unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote("t.tool", &def).unwrap()
}

#[tokio::test]
async fn methods_without_args_or_result_travel_without_them() {
    let _serial = serial().await;
    let server = named("server");
    let notes: Log = Log::default();
    let noted = notes.clone();
    let factory = move |_: &Json, _: &Json, _: &mut Declare| {
        let noted = noted.clone();
        Ok(Implementation::variant("main")
            .method("ping", |_, _| ready(Ok(Json::Null)))
            .method("count", |_, _| ready(Ok(json!(2))))
            .method("note", move |text, _| {
                noted.push(text.as_str().unwrap_or_default());
                ready(Ok(json!("dropped")))
            }))
    };
    server.serve(&tool(), factory, ServeOptions::default()).unwrap();
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    let sent: Log<Frame> = Log::default();
    let recording: Arc<dyn Link> = Arc::new(Recording { inner: up, sent: sent.clone() });
    let upstream = Some((recording, ConnectOptions::new(json!({"kind": "server"}))));
    let browser =
        create_host(HostOptions { kind: PeerKind::Client, upstream, ..host_options("browser", &Log::default()) });
    let tool = browser.hold(&tool(), json!({}), GetOptions::default()).unwrap();
    tool.ready().await.unwrap();
    assert_eq!(tool.call("ping", None, None).await.unwrap(), None);
    assert_eq!(tool.call("count", None, None).await.unwrap(), Some(json!(2)));
    assert_eq!(tool.call("note", Some(json!("hi")), None).await.unwrap(), None);
    assert_eq!(notes.all(), vec!["hi"]);
    let calls: Vec<bool> = sent
        .all()
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Call(call) => Some(call.args.is_some()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec![false, false, true]);
}

type Seen = Log<ProtocolVersions>;

fn mismatches() -> (Seen, MismatchHandler) {
    let seen: Seen = Log::default();
    let pushed = seen.clone();
    (seen, Arc::new(move |versions| pushed.push(versions)))
}

fn with_mismatch(handler: &MismatchHandler) -> LinkOptions {
    LinkOptions { on_protocol_mismatch: Some(handler.clone()), ..LinkOptions::default() }
}

#[tokio::test]
async fn a_peer_without_a_protocol_version_is_told_and_dropped_and_nothing_it_sends_is_served() {
    let _serial = serial().await;
    let server = named("server");
    serve_counter(&server, &Log::default());
    let (seen, handler) = mismatches();
    let (down, raw) = memory_link_pair();
    let accept = AcceptOptions { link: with_mismatch(&handler), ..AcceptOptions::client(json!({"guest": "g"})) };
    server.accept(down.clone(), accept).unwrap();
    let frames: Log<Frame> = Log::default();
    let (received, closed) = (frames.clone(), Latch::default());
    raw.receive(Arc::new(move |frame| {
        if let Received::Frame(frame) = frame {
            received.push(frame);
        }
    }));
    let on_close = closed.clone();
    raw.on_close(Box::new(move |reason| on_close.open(Json::from(reason))));
    let hello = Hello { protocol: None, name: "old".into(), kind: PeerKind::Client, code_version: 0, acks: None };
    raw.send(&Frame::Hello(hello));
    let hold = Hold {
        id: 1,
        at: "server".into(),
        token: counter().description().clone(),
        input: json!({"name": "c"}),
        principal: None,
        origin: None,
        state: true,
        again: false,
    };
    raw.send(&Frame::Hold(Box::new(hold)));
    assert_eq!(closed.wait().await, Json::from(format!("protocol-version {PROTOCOL_VERSION}")));
    assert_eq!(seen.all(), vec![ProtocolVersions { local: PROTOCOL_VERSION, peer: Some(1) }]);
    assert!(down.closed());
    sleep_ms(20).await;
    assert!(!frames.all().iter().any(|frame| matches!(frame, Frame::Shape(_))), "the old client's hold was not served");
}

#[tokio::test]
async fn a_client_learns_that_its_server_speaks_another_version_and_stops() {
    let _serial = serial().await;
    let (seen, handler) = mismatches();
    let (down, up) = memory_link_pair();
    let browser = create_host(HostOptions { kind: PeerKind::Client, ..host_options("browser", &Log::default()) });
    browser.connect(up.clone(), ConnectOptions { principal: json!({"kind": "server"}), link: with_mismatch(&handler) });
    // A newer server: its hello announces the next version, and it closes the link the same way Chord does.
    let next = PROTOCOL_VERSION + 1;
    let hello =
        Hello { protocol: Some(next), name: "server".into(), kind: PeerKind::Host, code_version: 1, acks: None };
    down.send(&Frame::Hello(hello));
    until(|| up.closed()).await;
    assert_eq!(seen.all(), vec![ProtocolVersions { local: PROTOCOL_VERSION, peer: Some(next) }]);

    // Told by the peer (it noticed first): the close reason carries its version.
    let (told, handler) = mismatches();
    let (down, up) = memory_link_pair();
    let other = create_host(HostOptions { kind: PeerKind::Client, ..host_options("browser", &Log::default()) });
    other.connect(up, ConnectOptions { principal: json!({"kind": "server"}), link: with_mismatch(&handler) });
    connected(&other, 1).await;
    down.close(Some(&format!("protocol-version {next}")));
    until(|| !told.is_empty()).await;
    assert_eq!(told.all(), vec![ProtocolVersions { local: PROTOCOL_VERSION, peer: Some(next) }]);
}

#[tokio::test]
async fn peers_of_the_same_version_talk_and_an_ordinary_close_is_no_mismatch() {
    let _serial = serial().await;
    let (seen, handler) = mismatches();
    let server = named("server");
    let (down, up) = memory_link_pair();
    let accept = AcceptOptions { link: with_mismatch(&handler), ..AcceptOptions::client(json!({"guest": "g"})) };
    server.accept(down, accept).unwrap();
    let browser = create_host(HostOptions { kind: PeerKind::Client, ..host_options("browser", &Log::default()) });
    browser.connect(up.clone(), ConnectOptions { principal: json!({"kind": "server"}), link: with_mismatch(&handler) });
    connected(&browser, 1).await;
    sleep_ms(10).await;
    up.close(Some("bye"));
    sleep_ms(20).await;
    assert!(seen.is_empty());
}

fn big() -> Contract {
    let notes = array(string());
    let main = VariantDef::new().state("notes", notes.clone()).method("echo", method(notes.clone(), notes));
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named("session:S1").unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote("t.big", &def).unwrap()
}

/// About 160,000 characters of JSON: above `PACK_THRESHOLD`.
fn large(tag: &str) -> Json {
    Json::array((0..40).map(|index| Json::from(format!("{tag}{index}:{}", "x".repeat(4000)))).collect())
}

#[tokio::test]
async fn large_args_results_and_snapshots_cross_a_relay_deflated_and_arrive_intact() {
    let _serial = serial().await;
    let server = named("server");
    let resolve: Resolve = Arc::new(|name: &str| {
        let (down, up) = memory_link_pair();
        let worker = child_on(name, &up, LinkOptions::default(), &Log::default());
        let factory = |_: &Json, _: &Json, decl: &mut Declare| {
            let notes = decl.state("notes", array(string()), large("a"))?;
            Ok(Implementation::variant("main").state("notes", notes).method("echo", |notes, _| ready(Ok(notes))))
        };
        worker.serve(&big(), factory, ServeOptions::default()).unwrap();
        let down: Arc<dyn Link> = down;
        Box::pin(ready(Ok(down)))
    });
    let authorize: Authorize = Arc::new(|_: &Json, _: &str| Box::pin(ready(true)));
    server.route("session:", Route { authorize, resolve, principal: Arc::new(|name: &str| json!({"host": name})) });
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"owner": true}))).unwrap();
    let browser = browser_on(&up, LinkOptions::default());
    let big = browser.hold(&big(), json!({}), GetOptions::default()).unwrap();
    assert_eq!(call(&big, "echo", large("c")).await.unwrap(), large("c"));
    let notes = big.state("notes").unwrap();
    let _listen = on_value(notes.as_ref(), |_| {});
    for _ in 0..100 {
        if notes.value().is_ok() {
            break;
        }
        sleep_ms(5).await;
    }
    assert_eq!(notes.value().unwrap(), large("a"));
}
