//! Port of upstream `test/bindings.test.ts`: shared bindings (traffic, linger, identity), hydration over a link,
//! method-only references and protocol version 4. The origins of client holds are in `chord2_bindings_origins.rs`.

mod chord2_support;

use std::collections::HashMap;
use std::future::ready;
use std::sync::Arc;
use std::time::Duration;

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler, pack_object};
use pi_chord::host2::object::Adapter;
use pi_chord::host2::wire::{Frame, Hello, PROTOCOL_VERSION, PeerKind, StateFrame, StateKind};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, GetOptions, Host, HostOptions, Implementation, LingerOptions,
    Link, LinkOptions, ReadableRef, Received, Ref, RefStatus, RemoteDef, RootOptions, ServeOptions, VariantDef,
    create_host, host_named, memory_link_pair, method, remote,
};
use pi_chord::{ChordResult, Json, json};

/// A link that records the frames crossing it, per direction (upstream `CountingLink`).
struct Counting {
    inner: Memory,
    sent: Log<Frame>,
    received: Log<Frame>,
}

impl Counting {
    fn wrap(inner: Memory) -> Arc<Self> {
        Arc::new(Self { inner, sent: Log::default(), received: Log::default() })
    }

    fn frames(&self, direction: &str) -> Vec<Frame> {
        if direction == "sent" { self.sent.all() } else { self.received.all() }
    }

    fn count(&self, direction: &str, tag: &str) -> usize {
        self.frames(direction).iter().filter(|frame| frame.tag() == tag).count()
    }

    /// Frames sent or received, except pings, pongs and acknowledgements.
    fn traffic(&self, direction: &str) -> usize {
        let quiet = |frame: &&Frame| !matches!(frame.tag(), "ping" | "pong" | "ack");
        self.frames(direction).iter().filter(quiet).count()
    }

    fn states(&self) -> Vec<StateFrame> {
        let state = |frame| if let Frame::State(state) = frame { Some(state) } else { None };
        self.received.all().into_iter().filter_map(state).collect()
    }

    /// The members of the snapshots received, sorted.
    fn snapshots(&self) -> Vec<String> {
        let snaps = self.states().into_iter().filter(|frame| frame.kind == StateKind::Snap);
        let mut members: Vec<String> = snaps.map(|frame| frame.m).collect();
        members.sort();
        members
    }
}

impl Link for Counting {
    fn closed(&self) -> bool {
        self.inner.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        self.sent.push(frame.clone());
        self.inner.send(frame)
    }

    fn receive(&self, handler: FrameHandler) {
        let received = self.received.clone();
        self.inner.receive(Arc::new(move |message| {
            if let Received::Frame(frame) = &message {
                received.push(frame.clone());
            }
            handler(message);
        }));
    }

    fn on_close(&self, handler: CloseHandler) {
        self.inner.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        self.inner.close(reason);
    }
}

/// Polls `check` every 2 ms, for at most about 2 s (upstream `until`).
async fn soon(check: impl Fn() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        sleep_ms(2).await;
    }
    assert!(check(), "condition not reached");
}

fn read(reference: &Ref, member: &str) -> ChordResult<Json> {
    reference.state(member).expect("a state member").value()
}

fn length(value: &Json) -> Json {
    Json::from(value.as_array().map_or(0, Vec::len) as f64)
}

fn notes_contract(version: i64, clear: bool) -> Contract {
    let input = object(&[opt("list", with(string(), serde_json::json!({"default": "main"})))]);
    let mut main = VariantDef::new()
        .state("notes", array(string()))
        .state("title", string())
        .method("add", method(string(), None))
        .method("rename", method(string(), None));
    if clear {
        main = main.method("clear", method(None, None));
    }
    let def = RemoteDef { version, input, host: host_named("server").unwrap(), variants: vec![("main".into(), main)] };
    remote("t.e8-notes", &def).unwrap()
}

fn notes() -> Contract {
    notes_contract(1, false)
}

fn serve_notes(host: &Host) {
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let notes = decl.state("notes", array(string()), json!([]))?;
        let title = decl.state("title", string(), json!("untitled"))?;
        let (list, name) = (notes.clone(), title.clone());
        Ok(Implementation::variant("main")
            .state("notes", notes)
            .state("title", title)
            .method("add", move |text, ctx| {
                let list = list.clone();
                async move { list.change(&ctx, |draft| draft.push(text)).map(|()| Json::Null) }
            })
            .method("rename", move |text, ctx| {
                let name = name.clone();
                async move { name.replace(&ctx, text).map(|()| Json::Null) }
            }))
    };
    host.serve(&notes(), factory, ServeOptions::default()).unwrap();
}

struct Setup {
    server: Host,
    browser: Host,
    link: Arc<Counting>,
}

/// A server serving `Notes`, and a browser whose end of the link counts what crosses it.
fn setup(linger: LingerOptions) -> Setup {
    let server = named("server");
    serve_notes(&server);
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"user": "u"}))).unwrap();
    let link = Counting::wrap(up);
    let upstream: Arc<dyn Link> = link.clone();
    let upstream = Some((upstream, ConnectOptions::new(json!({"kind": "server"}))));
    let options = HostOptions { kind: PeerKind::Client, upstream, linger, ..host_options("browser", &Log::default()) };
    Setup { server, browser: create_host(options), link }
}

async fn close(setup: Setup) {
    setup.browser.close().await.unwrap();
    setup.server.close().await.unwrap();
}

#[tokio::test]
async fn n_references_to_one_object_one_hold_one_snapshot_per_member_one_frame_per_change_one_release() {
    let _serial = serial().await;
    let s = setup(LingerOptions::default());
    let panel_type = local_type("t.e8-panel", object(&[]), vec![("count", number().into())]);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let notes = decl.get(&notes(), json!({}), GetOptions::default())?;
        let count = decl.derive(vec![notes.state("notes").unwrap()], |values| Ok(length(&values[0])));
        Ok(Implementation::new().state("count", count))
    };
    s.browser.serve(&panel_type, factory, ServeOptions::default()).unwrap();
    let a = s.browser.hold(&notes(), json!({}), GetOptions::default()).unwrap();
    let b = s.browser.hold(&notes(), json!({"list": "main"}), GetOptions::default()).unwrap();
    let panel = s.browser.hold(&panel_type, json!({}), GetOptions::default()).unwrap();
    for reference in [&a, &b, &panel] {
        reference.ready().await.unwrap();
    }
    let via_ctx = s.browser.root(
        |ctx| async move {
            let reference = ctx.get(&notes(), json!({}), GetOptions::default()).await?;
            read(&reference, "title")
        },
        RootOptions::default(),
    );
    assert_eq!(via_ctx.await.unwrap(), json!("untitled"));
    assert_eq!(s.link.count("sent", "hold"), 1, "one hold for every reference");
    assert_eq!(s.link.snapshots(), ["notes", "title"]);

    let before = s.link.states().len();
    call(&a, "add", json!("milk")).await.unwrap();
    let one = json!(1);
    soon(|| {
        let a = read(&a, "notes").map(|notes| length(&notes));
        let b = read(&b, "notes").map(|notes| length(&notes));
        a.ok() == Some(one.clone()) && b.ok() == Some(one.clone()) && read(&panel, "count").ok() == Some(one.clone())
    })
    .await;
    sleep_ms(20).await;
    assert_eq!(s.link.states().len() - before, 1, "one delta frame for one change, whatever N");

    let sent = s.link.traffic("sent");
    a.release();
    panel.release();
    sleep_ms(30).await;
    assert_eq!(s.link.traffic("sent"), sent, "releasing all but one reference sends nothing");
    b.release();
    soon(|| s.link.count("sent", "release") == 1).await;
    close(s).await;
}

#[tokio::test]
async fn a_held_reference_stays_subscribed_without_listeners_and_values_are_readable_at_once_after_ready() {
    let _serial = serial().await;
    let s = setup(LingerOptions::default());
    let notes = s.browser.hold(&notes(), json!({}), GetOptions::default()).unwrap();
    notes.ready().await.unwrap();
    assert_eq!(read(&notes, "notes").unwrap(), json!([]));
    call(&notes, "add", json!("bread")).await.unwrap();
    soon(|| read(&notes, "notes").ok() == Some(json!(["bread"]))).await;
    assert_eq!(read(&notes, "title").unwrap(), json!("untitled"));
    close(s).await;
}

fn linger_notes(ms: u64, cap: usize) -> LingerOptions {
    LingerOptions { types: HashMap::from([("t.e8-notes".to_string(), Duration::from_millis(ms))]), cap }
}

#[tokio::test]
async fn a_lingering_binding_keeps_its_hold_and_state_and_a_reference_taken_meanwhile_costs_nothing() {
    let _serial = serial().await;
    let s = setup(linger_notes(40, 4));
    let first = s.browser.get(&notes(), json!({}), GetOptions::default()).await.unwrap();
    let frames = s.link.traffic("received");
    first.release();
    sleep_ms(10).await;
    assert_eq!(s.link.count("sent", "release"), 0, "not released before the linger");
    // A hook mounting and unmounting: no frames either way.
    for _ in 0..3 {
        let again = s.browser.get(&notes(), json!({}), GetOptions::default()).await.unwrap();
        assert_eq!(read(&again, "notes").unwrap(), json!([]));
        again.release();
    }
    assert_eq!(s.link.count("sent", "hold"), 1);
    assert_eq!(s.link.traffic("received"), frames, "nothing new crossed the link");
    soon(|| s.link.count("sent", "release") == 1).await;
    close(s).await;
}

#[tokio::test]
async fn at_most_cap_bindings_linger_and_the_least_recently_released_goes_first() {
    let _serial = serial().await;
    let s = setup(linger_notes(10_000, 1));
    let a = s.browser.get(&notes(), json!({"list": "a"}), GetOptions::default()).await.unwrap();
    let b = s.browser.get(&notes(), json!({"list": "b"}), GetOptions::default()).await.unwrap();
    a.release();
    sleep_ms(5).await;
    assert_eq!(s.link.count("sent", "release"), 0);
    b.release();
    soon(|| s.link.count("sent", "release") == 1).await;
    let sent = s.link.sent.all();
    let released = sent.iter().find_map(|frame| if let Frame::Release { id } = frame { Some(*id) } else { None });
    let hold_of_a = sent.iter().find_map(|frame| match frame {
        Frame::Hold(hold) if hold.input["list"] == json!("a") => Some(hold.id),
        _ => None,
    });
    assert_eq!(released, hold_of_a);
    close(s).await;
}

#[tokio::test]
async fn normalized_equal_inputs_and_equal_principal_keys_share_and_other_principals_and_fingerprints_do_not() {
    let _serial = serial().await;
    let server = named("server");
    let notes_v2 = notes_contract(2, true);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let notes = decl.state("notes", array(string()), json!([]))?;
        let title = decl.state("title", string(), json!("untitled"))?;
        Ok(Implementation::variant("main")
            .state("notes", notes)
            .state("title", title)
            .method("add", |_, _| ready(Ok(Json::Null)))
            .method("rename", |_, _| ready(Ok(Json::Null)))
            .method("clear", |_, _| ready(Ok(Json::Null))))
    };
    let adapter: Adapter = Arc::new(|current: &Implementation| current.clone());
    let options = ServeOptions { accepts: Some(HashMap::from([(1, adapter)])), ..ServeOptions::default() };
    server.serve(&notes_v2, factory, options).unwrap();
    // A worker holds the server's objects; the principals it forwards key its bindings.
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::host("session:w", json!({"worker": "w"}))).unwrap();
    let link = Counting::wrap(up);
    let upstream: Arc<dyn Link> = link.clone();
    let upstream = Some((upstream, ConnectOptions::new(json!({"kind": "server"}))));
    let worker = create_host(HostOptions { upstream, ..host_options("session:w", &Log::default()) });
    let as_user = |principal: Json| GetOptions::principal(principal);
    let refs = [
        worker.hold(&notes(), json!({}), GetOptions::default()),
        worker.hold(&notes(), json!({"list": "main"}), GetOptions::default()),
        worker.hold(&notes(), json!({}), as_user(json!({"user": "a", "grant": 1}))),
        worker.hold(&notes(), json!({}), as_user(json!({"grant": 1, "user": "a"}))),
        worker.hold(&notes(), json!({}), as_user(json!({"user": "a", "grant": 2}))),
        worker.hold(&notes_v2, json!({}), GetOptions::default()),
    ];
    for reference in &refs {
        reference.as_ref().unwrap().ready().await.unwrap();
    }
    // host (shared by two inputs), user a grant 1 (shared by two key orders), user a grant 2, the v2 contract.
    assert_eq!(link.count("sent", "hold"), 4);
    worker.close().await.unwrap();
    server.close().await.unwrap();
}

fn matrix() -> Contract {
    let values = VariantDef::new()
        .state("none", json!({"type": "null"}))
        .state("empty", array(string()))
        .state("maybe", unknown());
    let bare = VariantDef::new().method("ping", method(None, string()));
    let broken = VariantDef::new().state("n", number());
    let variants = vec![("values".into(), values), ("bare".into(), bare), ("broken".into(), broken)];
    let input = object(&[req("variant", string())]);
    remote("t.e8-matrix", &RemoteDef { version: 1, input, host: host_named("server").unwrap(), variants }).unwrap()
}

fn serve_matrix(host: &Host) {
    let factory = |input: &Json, _: &Json, decl: &mut Declare| match input["variant"].as_str() {
        Some("bare") => Ok(Implementation::variant("bare").method("ping", |_, _| ready(Ok(json!("pong"))))),
        Some("broken") => {
            let source = decl.state("source", string(), json!("not a number"))?;
            // A derivation exposed under a number schema: its snapshot fails validation.
            let n = decl.derive(vec![source as ReadableRef], |values| Ok(values[0].clone()));
            Ok(Implementation::variant("broken").state("n", n))
        }
        _ => {
            let none = decl.state("none", json!({"type": "null"}), Json::Null)?;
            let empty = decl.state("empty", array(string()), json!([]))?;
            let maybe = decl.derive(vec![none.clone() as ReadableRef], |_| Ok(Json::Null));
            Ok(Implementation::variant("values").state("none", none).state("empty", empty).state("maybe", maybe))
        }
    };
    host.serve(&matrix(), factory, ServeOptions::default()).unwrap();
}

#[tokio::test]
async fn null_empty_and_undefined_values_hydrate_a_state_free_variant_is_ready_a_failed_snapshot_rejects() {
    let _serial = serial().await;
    let server = named("server");
    serve_matrix(&server);
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"user": "u"}))).unwrap();
    let browser = browser_on(&up, LinkOptions::default());
    let values = browser.get(&matrix(), json!({"variant": "values"}), GetOptions::default()).await.unwrap();
    assert_eq!(values.variant().as_deref(), Some("values"));
    assert_eq!(read(&values, "none").unwrap(), Json::Null);
    assert_eq!(read(&values, "empty").unwrap(), json!([]));
    assert_eq!(read(&values, "maybe").unwrap(), Json::Null);
    let bare = browser.get(&matrix(), json!({"variant": "bare"}), GetOptions::default()).await.unwrap();
    assert_eq!(bare.variant().as_deref(), Some("bare"));
    assert_eq!(call(&bare, "ping", Json::Null).await.unwrap(), json!("pong"));
    // `none` is not a member of the bare variant.
    let error = read(&bare, "none").unwrap_err();
    assert!(is(&error, "not_in_variant"), "{error}");
    let broken = browser.hold(&matrix(), json!({"variant": "broken"}), GetOptions::default()).unwrap();
    let error = broken.ready().await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    assert_eq!(broken.status(), RefStatus::Gone);
    browser.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn reading_before_the_first_hydration_throws_not_ready() {
    let _serial = serial().await;
    let s = setup(LingerOptions::default());
    let notes = s.browser.hold(&notes(), json!({}), GetOptions::default()).unwrap();
    let error = read(&notes, "notes").unwrap_err();
    assert!(is(&error, "not_ready"), "{error}");
    notes.ready().await.unwrap();
    assert_eq!(read(&notes, "notes").unwrap(), json!([]));
    close(s).await;
}

#[tokio::test]
async fn method_only_references_transfer_no_state_alone_and_a_full_reference_starts_and_stops_it() {
    let _serial = serial().await;
    let s = setup(LingerOptions::default());
    let method_only = GetOptions { state: Some(false), ..GetOptions::default() };
    let caller = s.browser.hold(&notes(), json!({}), method_only).unwrap();
    caller.ready().await.unwrap();
    call(&caller, "add", json!("x")).await.unwrap();
    let hold =
        s.link.sent.all().into_iter().find_map(|frame| if let Frame::Hold(hold) = frame { Some(hold) } else { None });
    assert!(hold.is_some_and(|hold| !hold.state));
    assert!(s.link.states().is_empty());
    let error = read(&caller, "notes").unwrap_err();
    assert!(is(&error, "not_ready"), "{error}");

    let viewer = s.browser.hold(&notes(), json!({}), GetOptions::default()).unwrap();
    viewer.ready().await.unwrap();
    assert_eq!(read(&viewer, "notes").unwrap(), json!(["x"]));
    assert_eq!(s.link.count("sent", "sub"), 1);
    assert_eq!(s.link.count("sent", "hold"), 1, "the same hold");
    viewer.release();
    soon(|| s.link.count("sent", "unsub") == 1).await;
    let after = s.link.states().len();
    call(&caller, "add", json!("y")).await.unwrap();
    sleep_ms(20).await;
    assert_eq!(s.link.states().len(), after, "no state once only method-only references remain");
    close(s).await;
}

#[tokio::test]
async fn a_version_3_peer_is_refused_and_shapes_carry_no_code_version() {
    let _serial = serial().await;
    let s = setup(LingerOptions::default());
    s.browser.hold(&notes(), json!({}), GetOptions::default()).unwrap().ready().await.unwrap();
    let shape = s.link.received.all().into_iter().find(|frame| frame.tag() == "shape").expect("a shape");
    assert!(pack_object(&shape, false).get("codeVersion").is_none());
    let Frame::Shape(mut shape) = shape else { unreachable!() };
    shape.state.sort();
    assert_eq!(shape.state, ["notes", "title"]);

    let (down, raw) = memory_link_pair();
    s.server.accept(down, AcceptOptions::client(json!({"user": "old"}))).unwrap();
    let closed = Latch::default();
    let on_close = closed.clone();
    raw.on_close(Box::new(move |reason| on_close.open(Json::from(reason))));
    raw.receive(Arc::new(|_| {}));
    let hello = Hello { protocol: Some(3), name: "old".into(), kind: PeerKind::Client, code_version: 0, acks: None };
    raw.send(&Frame::Hello(hello));
    let reason = tokio::time::timeout(Duration::from_secs(5), closed.wait()).await.expect("the link closes");
    assert_eq!(reason, Json::from(format!("protocol-version {PROTOCOL_VERSION}")));
    assert_eq!(PROTOCOL_VERSION, 4);
    close(s).await;
}
