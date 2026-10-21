//! Port of upstream `test/flow-units.test.ts`: flow control with units (urgent first units, whole snapshots,
//! unsized links, the per-link cap of waiting ops) and the relay topology. Every case is ported.

mod chord2_support;

use std::sync::{Arc, Mutex};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::{Frame, PeerKind, StateFrame, StateKind};
use pi_chord::host2::{
    AcceptOptions, Authorize, ConnectOptions, Contract, Declare, GetOptions, Host, HostOptions, Implementation, Link,
    LinkOptions, PrincipalMatch, Readable, Received, Ref, RemoteDef, Resolve, Role, Route, ServeOptions, VariantDef,
    create_host, host_family, host_named, memory_link_pair, method, remote,
};
use pi_chord::{Json, json};

/// Counts what crosses a link, per direction, and can hold back the acknowledgements its own side sends.
struct CountingLink {
    inner: Arc<dyn Link>,
    sent: Mutex<Vec<Frame>>,
    received: Arc<Mutex<Vec<Frame>>>,
    /// Whether acks are held back, and the latest one held.
    gate: Mutex<(bool, Option<Frame>)>,
}

impl CountingLink {
    fn new(inner: Arc<dyn Link>) -> Arc<Self> {
        Arc::new(Self { inner, sent: Mutex::default(), received: Arc::default(), gate: Mutex::default() })
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

    /// Frames of type `tag` this side sent.
    fn count_sent(&self, tag: &str) -> usize {
        self.sent.lock().unwrap().iter().filter(|frame| frame.tag() == tag).count()
    }
}

impl Link for CountingLink {
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
            self.sent.lock().unwrap().push(frame.clone());
            return self.inner.send(frame);
        }
        self.sent.lock().unwrap().push(frame.clone());
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

/// A link that reports no sizes, as Node's IPC channel does.
struct Unsized(Arc<dyn Link>);

impl Link for Unsized {
    fn closed(&self) -> bool {
        self.0.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        self.0.send(frame);
        None
    }

    fn receive(&self, handler: FrameHandler) {
        self.0.receive(handler);
    }

    fn on_close(&self, handler: CloseHandler) {
        self.0.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        self.0.close(reason);
    }
}

fn blob() -> Contract {
    let main = VariantDef::new()
        .state("lines", array(string()))
        .method("push", method(string(), None))
        .method("ping", method(None, string()));
    let input = object(&[req("name", string())]);
    let def =
        RemoteDef { version: 1, input, host: host_named("server").unwrap(), variants: vec![("main".into(), main)] };
    remote("t.e8-blob", &def).unwrap()
}

/// About `kib` KiB of lines.
fn lines(kib: usize, tag: &str) -> Json {
    Json::array((0..kib).map(|i| Json::from(format!("{tag}{i}:{}", "x".repeat(1000)))).collect())
}

fn serve_blob(host: &Host, size: fn(&str) -> usize) {
    let factory = move |input: &Json, _: &Json, decl: &mut Declare| {
        let name = input["name"].as_str().unwrap_or_default();
        let state = decl.state("lines", array(string()), lines(size(name), name))?;
        let pusher = state.clone();
        Ok(Implementation::variant("main")
            .state("lines", state)
            .method("push", move |line, ctx| {
                let state = pusher.clone();
                async move {
                    state.change(&ctx, |draft| draft.push(line))?;
                    Ok(Json::Null)
                }
            })
            .method("ping", |_, _| std::future::ready(Ok(json!("pong")))))
    };
    host.serve(&blob(), factory, ServeOptions::default()).unwrap();
}

fn client_host(link: Arc<dyn Link>, name: &str) -> Host {
    let upstream = Some((link, ConnectOptions::new(json!({"kind": "server"}))));
    create_host(HostOptions { kind: PeerKind::Client, upstream, ..host_options(name, &Log::default()) })
}

fn accept_options(principal: Json, window: Option<usize>, pending_cap: Option<usize>) -> AcceptOptions {
    let link = LinkOptions { window, pending_cap, ..LinkOptions::default() };
    AcceptOptions { link, ..AcceptOptions::client(principal) }
}

fn client(server: &Host, window: usize, pending_cap: Option<usize>) -> (Host, Arc<CountingLink>) {
    let (down, up) = memory_link_pair();
    server.accept(down, accept_options(json!({"user": "u"}), Some(window), pending_cap)).unwrap();
    let link = CountingLink::new(up);
    (client_host(link.clone(), "browser"), link)
}

fn state_frames(frames: &[Frame]) -> Vec<StateFrame> {
    let state =
        frames.iter().filter_map(|frame| if let Frame::State(state) = frame { Some(state.clone()) } else { None });
    state.collect()
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

fn line_count(reference: &Ref) -> usize {
    let value = reference.state("lines").and_then(|lines| lines.value().ok());
    value.and_then(|value| value.as_array().map(Vec::len)).unwrap_or_default()
}

fn hold_blob(host: &Host, name: &str) -> Ref {
    host.hold(&blob(), json!({"name": name}), GetOptions::default()).unwrap()
}

#[tokio::test]
async fn with_large_streams_behind_a_full_window_a_new_get_resolves_at_once_and_calls_are_answered() {
    let _serial = serial().await;
    let server = named("server");
    serve_blob(&server, |name| if name == "small" { 1 } else { 500 });
    let (host, link) = client(&server, 64 * 1024, None);
    let bigs: Vec<Ref> = ["a", "b", "c"].iter().map(|name| hold_blob(&host, name)).collect();
    for big in &bigs {
        big.ready().await.unwrap();
    }
    link.pause();
    // Changes to every large stream now wait for acknowledgements that do not come.
    for big in &bigs {
        for i in 0..50 {
            call(big, "push", json!(format!("{i}:{}", "y".repeat(2000)))).await.unwrap();
        }
    }
    let small = hold_blob(&host, "small");
    small.ready().await.unwrap();
    assert_eq!(line_count(&small), 1);
    assert_eq!(call(&bigs[0], "ping", Json::Null).await.unwrap(), json!("pong"));
    assert!(line_count(&bigs[0]) < 550, "the large streams still wait");
    link.release();
    settle(|| bigs.iter().all(|big| line_count(big) == 550)).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_snapshot_larger_than_the_window_arrives_whole_and_later_changes_wait_for_acknowledgements() {
    let _serial = serial().await;
    let server = named("server");
    serve_blob(&server, |_| 300);
    let (host, link) = client(&server, 16 * 1024, None);
    link.pause();
    let big = hold_blob(&host, "chat");
    big.ready().await.unwrap();
    assert_eq!(line_count(&big), 300);
    for i in 0..40 {
        call(&big, "push", json!(format!("{i}:{}", "z".repeat(1000)))).await.unwrap();
    }
    assert!(line_count(&big) < 340);
    link.release();
    settle(|| line_count(&big) == 340).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_link_that_reports_no_sizes_never_holds_state_back() {
    let _serial = serial().await;
    let server = named("server");
    serve_blob(&server, |_| 1);
    let (down, up) = memory_link_pair();
    server.accept(Arc::new(Unsized(down)), accept_options(json!({"user": "u"}), Some(1), None)).unwrap();
    let link = CountingLink::new(up);
    let host = client_host(link.clone(), "worker");
    link.pause();
    let blob = hold_blob(&host, "x");
    blob.ready().await.unwrap();
    for i in 0..20 {
        call(&blob, "push", json!(format!("{i}"))).await.unwrap();
    }
    settle(|| line_count(&blob) == 21).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn waiting_ops_stay_under_the_per_link_cap_however_many_streams_stall_and_each_then_gets_one_reset() {
    let _serial = serial().await;
    let server = named("server");
    serve_blob(&server, |_| 1);
    let cap = 8 * 1024;
    let (host, link) = client(&server, 1024, Some(cap));
    let blobs: Vec<Ref> = (0..6).map(|i| hold_blob(&host, &format!("s{i}"))).collect();
    for blob in &blobs {
        blob.ready().await.unwrap();
    }
    link.pause();
    let mut peak = 0;
    for round in 0..30 {
        for blob in &blobs {
            call(blob, "push", json!(format!("{round}:{}", "w".repeat(200)))).await.unwrap();
        }
        let connections = server.connections().await;
        let connection = connections.iter().find(|connection| connection.role == Role::Downstream).unwrap();
        peak = peak.max(connection.waiting.bytes);
    }
    assert!(peak <= cap, "waiting ops {peak} exceed the cap {cap}");
    let before = link.received().len();
    link.release();
    settle(|| blobs.iter().all(|blob| line_count(blob) == 31)).await;
    let received = link.received();
    let resets: Vec<StateFrame> =
        state_frames(&received[before..]).into_iter().filter(|frame| frame.kind == StateKind::Reset).collect();
    let mut per = std::collections::HashMap::<u64, usize>::new();
    for frame in &resets {
        *per.entry(frame.hold).or_default() += 1;
    }
    assert!(!resets.is_empty(), "dropped streams came back as resets");
    assert!(per.values().all(|count| *count <= 2), "about one reset per stream: {per:?}");
    server.close().await.unwrap();
}

fn relayed_counter() -> Contract {
    let main = VariantDef::new().state("n", number()).method("add", method(number(), None));
    let input = object(&[req("session", string())]);
    let host = host_family("session:", "session").unwrap();
    remote("t.e8-relayed", &RemoteDef { version: 1, input, host, variants: vec![("main".into(), main)] }).unwrap()
}

fn serve_relayed(host: &Host) {
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let n = decl.state("n", number(), json!(0))?;
        let adder = n.clone();
        Ok(Implementation::variant("main").state("n", n).method("add", move |delta, ctx| {
            let n = adder.clone();
            async move {
                let value = n.value()?.as_f64().unwrap_or_default() + delta.as_f64().unwrap_or_default();
                n.replace(&ctx, json!(value))?;
                Ok(Json::Null)
            }
        }))
    };
    host.serve(&relayed_counter(), factory, ServeOptions::default()).unwrap();
}

struct RelayCluster {
    server: Host,
    worker: Slot<Host>,
    to_worker: Slot<Arc<CountingLink>>,
}

type Slot<T> = Arc<Mutex<Option<T>>>;

struct Tab {
    host: Host,
    link: Arc<CountingLink>,
    up: Arc<pi_chord::host2::MemoryLink>,
}

impl RelayCluster {
    fn new() -> Self {
        let server = named("server");
        let (worker, to_worker): (Slot<Host>, Slot<Arc<CountingLink>>) = Default::default();
        let (spawned, link) = (worker.clone(), to_worker.clone());
        let resolve: Resolve = Arc::new(move |name: &str| {
            let mut link = link.lock().unwrap();
            if link.is_none() {
                let (down, up) = memory_link_pair();
                let created = child_on(name, &up, LinkOptions::default(), &Log::default());
                serve_relayed(&created);
                *spawned.lock().unwrap() = Some(created);
                *link = Some(CountingLink::new(down));
            }
            let resolved: Arc<dyn Link> = link.clone().unwrap();
            Box::pin(std::future::ready(Ok(resolved)))
        });
        let authorize: Authorize = Arc::new(|_: &Json, _: &str| Box::pin(std::future::ready(true)));
        let principal = Arc::new(|name: &str| json!({"host": name}));
        server.route("session:", Route { authorize, resolve, principal });
        Self { server, worker, to_worker }
    }

    fn browser(&self, principal: Json, window: Option<usize>) -> Tab {
        let (down, up) = memory_link_pair();
        let options = accept_options(principal, window, window.map(|window| window * 2));
        self.server.accept(down, options).unwrap();
        let link = CountingLink::new(up.clone());
        Tab { host: client_host(link.clone(), "browser"), link, up }
    }

    fn to_worker(&self) -> Arc<CountingLink> {
        self.to_worker.lock().unwrap().clone().unwrap()
    }

    /// The holds the worker serves for its parent.
    async fn served(&self) -> usize {
        let Some(worker) = self.worker.lock().unwrap().clone() else { return 0 };
        let connections = worker.connections().await;
        connections.iter().filter(|c| c.role == Role::Upstream).map(|c| c.served.len()).sum()
    }

    /// The server's relayed holds on its links to hosts.
    async fn relayed(&self) -> usize {
        let connections = self.server.connections().await;
        connections.iter().filter(|c| c.peer_kind == PeerKind::Host).map(|c| c.relayed_refs).sum()
    }

    async fn counts_reach(&self, served: usize, relayed: usize) {
        for _ in 0..2500 {
            if self.served().await == served && self.relayed().await == relayed {
                return;
            }
            sleep_ms(2).await;
        }
        assert_eq!((self.served().await, self.relayed().await), (served, relayed));
    }
}

fn hold_session(host: &Host) -> Ref {
    host.hold(&relayed_counter(), json!({"session": "S1"}), GetOptions::default()).unwrap()
}

fn n_of(reference: &Ref) -> f64 {
    let value = reference.state("n").and_then(|n| n.value().ok());
    value.and_then(|value| value.as_f64()).unwrap_or(-1.0)
}

#[tokio::test]
async fn each_origin_hold_is_one_downstream_hold_and_nothing_is_left_after_release_close_or_revoke() {
    let _serial = serial().await;
    let c = RelayCluster::new();
    // Two tabs of one guest (two connections, one identity) and another guest.
    let one = c.browser(json!({"guest": "g"}), None);
    let two = c.browser(json!({"guest": "g"}), None);
    let three = c.browser(json!({"guest": "h"}), None);
    let a = hold_session(&one.host);
    let a2 = hold_session(&one.host);
    let b = hold_session(&two.host);
    let d = hold_session(&three.host);
    for reference in [&a, &a2, &b, &d] {
        reference.ready().await.unwrap();
    }
    assert_eq!(c.to_worker().count_sent("hold"), 3, "two tabs and another guest: three holds");
    assert_eq!(c.served().await, 3);
    a.release();
    a2.release();
    c.counts_reach(2, 2).await;
    two.up.close(None);
    c.counts_reach(1, 1).await;
    c.server.invalidate_principal(PrincipalMatch::Pattern(json!({"guest": "h"}))).await.unwrap();
    c.counts_reach(0, 0).await;
    c.server.close().await.unwrap();
}

#[tokio::test]
async fn a_stalled_tab_overflows_and_gets_a_reset_while_another_tab_keeps_getting_every_change() {
    let _serial = serial().await;
    let c = RelayCluster::new();
    let slow = c.browser(json!({"guest": "g"}), Some(256));
    let fast = c.browser(json!({"guest": "g"}), None);
    let stalled = hold_session(&slow.host);
    let live = hold_session(&fast.host);
    stalled.ready().await.unwrap();
    live.ready().await.unwrap();
    slow.link.pause();
    let seen: Log<Json> = Log::default();
    let sink = seen.clone();
    let _seen = pi_chord::host2::on_value(live.state("n").unwrap().as_ref(), move |value| sink.push(value.clone()));
    for _ in 0..200 {
        call(&live, "add", json!(1)).await.unwrap();
    }
    settle(|| n_of(&live) == 200.0).await;
    assert!(seen.len() > 100, "the fast tab saw the changes as they came");
    assert!(n_of(&stalled) < 200.0, "the stalled tab waits");
    assert!(c.to_worker().count_sent("resync") >= 1, "the relay asked the worker for a reset");
    slow.link.release();
    settle(|| n_of(&stalled) == 200.0).await;
    c.server.close().await.unwrap();
}
