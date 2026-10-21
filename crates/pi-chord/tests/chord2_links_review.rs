//! Port of upstream `test/links-review.test.ts`. Left out: "#1 root_end is honored only from the link the root
//! came over" and the "cross-host reload" cases (#3, #4, #5, #6). They check leases and code versions with
//! `reload`, which host2 does not have yet (phase 4).

mod chord2_support;

use std::collections::HashMap;
use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler, parse_frame};
use pi_chord::host2::object::{Adapter, Member};
use pi_chord::host2::types::{EventListener, ValueListener};
use pi_chord::host2::wire::{Call, CallResult, Hello, Hold, Payload, PeerKind};
use pi_chord::host2::{
    AcceptOptions, Authorize, ConnectionInfo, Contract, Ctx, Declare, Frame, GetOptions, Host, Implementation, Link,
    LinkOptions, PROTOCOL_VERSION, PrincipalMatch, Readable, ReadableRef, Received, RefStatus, RemoteDef, Resolve,
    Restamp, Route, ServeOptions, StateEvent, VariantDef, host_named, memory_link_pair, method, on_event, on_value,
    remote,
};
use pi_chord::{ChordResult, Json, Unsubscribe, json};

/// A link that logs what it sends.
struct WatchedLink {
    inner: Memory,
    sent: Log<Frame>,
}

impl Link for WatchedLink {
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

/// Upstream's `Counter`: on `session:S1`, with a `slow` method that waits for the cluster's slow gate.
fn review_counter() -> Contract {
    let main = VariantDef::new()
        .state("count", object(&[req("n", number())]))
        .method("add", method(number(), number()))
        .method("who", method(None, unknown()))
        .method("slow", method(None, None));
    let input = object(&[req("name", string())]);
    let host = host_named("session:S1").unwrap();
    remote("t.counter", &RemoteDef { version: 1, input, host, variants: vec![("main".into(), main)] }).unwrap()
}

fn hold_frame(id: u64, name: &str) -> Frame {
    Frame::Hold(Box::new(Hold {
        id,
        at: "session:S1".into(),
        token: review_counter().description().clone(),
        input: json!({"name": name}),
        principal: None,
        origin: None,
        state: true,
        again: false,
    }))
}

#[derive(Clone)]
struct Worker {
    #[allow(dead_code)]
    host: Host,
    link: Arc<WatchedLink>,
}

type Workers = Arc<Mutex<HashMap<String, Worker>>>;

/// A server routing `session:*` to workers it spawns once per name; the server-to-worker link is watched.
struct Cluster {
    server: Host,
    log: Log,
    principals: Log<Json>,
    workers: Workers,
    resolve_gate: Arc<Mutex<Option<Latch>>>,
}

struct Browser {
    host: Host,
    up: Memory,
}

/// A client that speaks frames directly: they reach the server end at once, in order.
struct Raw {
    down: Memory,
    raw: Memory,
    frames: Log<Frame>,
}

impl Raw {
    fn send(&self, frame: Frame) {
        self.down.deliver(Received::Frame(frame));
    }

    fn send_text(&self, text: &str) {
        self.down.deliver(parse_frame(text).unwrap());
    }

    fn closed(&self) -> bool {
        self.raw.closed()
    }

    fn result(&self, id: u64) -> Option<CallResult> {
        self.frames.all().into_iter().find_map(|frame| match frame {
            Frame::Result(result) if result.id == id => Some(result),
            _ => None,
        })
    }
}

/// Spawns (or finds) the worker named `name` and returns the server end of its link.
fn spawn_worker(name: &str, workers: &Workers, log: &Log, principals: &Log<Json>, slow_gate: &Latch) -> Arc<dyn Link> {
    if let Some(existing) = workers.lock().unwrap().get(name) {
        return existing.link.clone();
    }
    let (down, up) = memory_link_pair();
    let link = Arc::new(WatchedLink { inner: down, sent: Log::default() });
    let host = child_on(name, &up, LinkOptions::default(), &Log::default());
    let (log, principals, slow_gate) = (log.clone(), principals.clone(), slow_gate.clone());
    let factory = move |input: &Json, principal: &Json, decl: &mut Declare| {
        principals.push(principal.clone());
        let (aborted, gate) = (log.clone(), slow_gate.clone());
        let slow = move |_: Json, ctx: Ctx| {
            let (log, gate) = (aborted.clone(), gate.clone());
            async move {
                ctx.signal().add_listener(move |_| log.push("slow aborted"));
                gate.wait().await;
                Ok(Json::Null)
            }
        };
        Ok(counter_impl(input, principal, decl, Some(&log))?.method("slow", slow))
    };
    host.serve(&review_counter(), factory, ServeOptions::default()).unwrap();
    workers.lock().unwrap().insert(name.to_string(), Worker { host, link: link.clone() });
    link
}

impl Cluster {
    fn new() -> Self {
        let state = Self {
            server: named("server"),
            log: Log::default(),
            principals: Log::default(),
            workers: Arc::default(),
            resolve_gate: Arc::default(),
        };
        let slow_gate = Latch::default();
        let (workers, log, principals) = (state.workers.clone(), state.log.clone(), state.principals.clone());
        let gate = state.resolve_gate.clone();
        let authorize: Authorize = Arc::new(|_: &Json, _: &str| Box::pin(ready(true)));
        let resolve: Resolve = Arc::new(move |name: &str| {
            let (workers, log, principals, slow_gate) =
                (workers.clone(), log.clone(), principals.clone(), slow_gate.clone());
            let (gate, name) = (gate.lock().unwrap().clone(), name.to_string());
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.wait().await;
                }
                Ok(spawn_worker(&name, &workers, &log, &principals, &slow_gate))
            })
        });
        let principal = Arc::new(|name: &str| json!({"host": name}));
        state.server.route("session:", Route { authorize, resolve, principal });
        state
    }

    /// Route resolutions wait until the returned latch opens.
    fn gate_resolves(&self) -> Latch {
        let gate = Latch::default();
        *self.resolve_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    fn browser(&self, principal: Json, restamp: Option<Restamp>) -> Browser {
        let (down, up) = memory_link_pair();
        self.server.accept(down, AcceptOptions { restamp, ..AcceptOptions::client(principal) }).unwrap();
        Browser { host: browser_on(&up, LinkOptions::default()), up }
    }

    fn raw(&self, principal: Json) -> Raw {
        let (down, raw) = memory_link_pair();
        self.server.accept(down.clone(), AcceptOptions::client(principal)).unwrap();
        let frames: Log<Frame> = Log::default();
        let seen = frames.clone();
        raw.receive(Arc::new(move |received| {
            if let Received::Frame(frame) = received {
                seen.push(frame);
            }
        }));
        let peer = Raw { down, raw, frames };
        let hello = Hello {
            protocol: Some(PROTOCOL_VERSION),
            name: "raw".into(),
            kind: PeerKind::Client,
            code_version: 0,
            acks: None,
        };
        peer.send(Frame::Hello(hello));
        peer
    }

    /// What the server sent to worker `session:S1` (nothing if it was never spawned).
    fn sent_to_worker(&self) -> Vec<Frame> {
        self.workers.lock().unwrap().get("session:S1").map(|worker| worker.link.sent.all()).unwrap_or_default()
    }

    fn logged(&self, entry: &str) -> usize {
        self.log.all().iter().filter(|logged| *logged == entry).count()
    }
}

/// The server's connection to worker `session:S1`.
async fn worker_link(server: &Host) -> ConnectionInfo {
    let connections = server.connections().await;
    let found = connections.into_iter().find(|connection| connection.name.as_deref() == Some("session:S1"));
    found.expect("a connection to session:S1")
}

/// Polls the server's connection to `session:S1` until `check` holds.
async fn until_worker_link(server: &Host, check: impl Fn(&ConnectionInfo) -> bool) {
    for _ in 0..500 {
        if check(&worker_link(server).await) {
            return;
        }
        sleep_ms(2).await;
    }
    panic!("condition not reached");
}

fn hold(host: &Host, name: &str) -> pi_chord::host2::Ref {
    host.hold(&review_counter(), json!({"name": name}), GetOptions::default()).unwrap()
}

// links review: identity and stamping

#[tokio::test]
async fn the_acceptor_decides_what_a_peer_is_a_client_announcing_itself_as_a_host_is_cut_off() {
    let _serial = serial().await;
    let c = Cluster::new();
    let (down, raw) = memory_link_pair();
    c.server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    raw.receive(Arc::new(|_| {}));
    let hello = Hello { protocol: None, name: "session:S9".into(), kind: PeerKind::Host, code_version: 1, acks: None };
    raw.send(&Frame::Hello(hello));
    until(|| raw.closed()).await;
    // A host link must have a name and its hello must match it.
    let unnamed = AcceptOptions { kind: PeerKind::Host, ..AcceptOptions::client(json!({})) };
    let error = c.server.accept(memory_link_pair().0, unnamed).unwrap_err();
    assert!(error.to_string().contains("needs its name"), "{error}");
}

#[tokio::test]
async fn a_revocation_while_the_route_is_still_resolving_closes_the_relay_and_no_worker_object_is_made() {
    let _serial = serial().await;
    let c = Cluster::new();
    let gate = c.gate_resolves();
    let restamp: Restamp =
        Arc::new(|principal: &Json| Some(json!({"guest": principal["guest"].clone(), "revision": 2})));
    let browser = c.browser(json!({"guest": "g", "revision": 1}), Some(restamp));
    let counter = hold(&browser.host, "a");
    sleep_ms(5).await;
    c.server.invalidate_principal(PrincipalMatch::Pattern(json!({"guest": "g"}))).await.unwrap();
    until(|| counter.status() == RefStatus::Gone).await;
    gate.open(Json::Null);
    sleep_ms(10).await;
    assert_eq!(c.principals.all(), Vec::<Json>::new());
    let again = hold(&browser.host, "a");
    assert_eq!(call(&again, "who", Json::Null).await.unwrap(), json!({"guest": "g", "revision": 2}));
}

#[tokio::test]
async fn malformed_frames_and_duplicate_ids_close_the_link() {
    let _serial = serial().await;
    let c = Cluster::new();
    let first = c.raw(json!({"guest": "g"}));
    first.send_text(r#"{"t":"call","hold":1}"#);
    until(|| first.closed()).await;
    let second = c.raw(json!({"guest": "g"}));
    second.send(hold_frame(1, "a"));
    second.send(hold_frame(1, "b"));
    until(|| second.closed()).await;
}

// links review: relay ownership

#[tokio::test]
async fn a_browser_disconnect_cancels_its_call_downstream() {
    let _serial = serial().await;
    let c = Cluster::new();
    let browser = c.browser(json!({"guest": "g"}), None);
    let counter = hold(&browser.host, "a");
    counter.ready().await.unwrap();
    let slow = counter.clone();
    tokio::spawn(async move {
        let _ = slow.call("slow", None, None).await;
    });
    sleep_ms(10).await;
    browser.up.close(None);
    until(|| c.logged("slow aborted") > 0).await;
    assert_eq!(worker_link(&c.server).await.relayed_calls, 0);
}

#[tokio::test]
async fn state_stopped_while_the_route_resolves_is_forwarded_as_a_hold_without_state() {
    let _serial = serial().await;
    let c = Cluster::new();
    let gate = c.gate_resolves();
    let raw = c.raw(json!({"guest": "g"}));
    raw.send(hold_frame(1, "a"));
    raw.send(Frame::Unsub { hold: 1 });
    sleep_ms(5).await;
    gate.open(Json::Null);
    until(|| c.sent_to_worker().iter().any(|frame| matches!(frame, Frame::Hold(_)))).await;
    sleep_ms(10).await;
    let sent = c.sent_to_worker();
    let held = sent.iter().find_map(|frame| match frame {
        Frame::Hold(hold) => Some(hold),
        _ => None,
    });
    assert!(held.is_some_and(|hold| !hold.state), "{sent:?}");
    assert!(!sent.iter().any(|frame| matches!(frame, Frame::Sub { .. } | Frame::Unsub { .. })), "{sent:?}");
}

#[tokio::test]
async fn a_release_while_the_route_resolves_settles_waiting_calls_and_forwards_nothing() {
    let _serial = serial().await;
    let c = Cluster::new();
    let gate = c.gate_resolves();
    let raw = c.raw(json!({"guest": "g"}));
    raw.send(hold_frame(1, "a"));
    let args = Some(Payload::Value(json!(1)));
    raw.send(Frame::Call(Call { id: 2, hold: 1, member: "add".into(), args, root: None }));
    sleep_ms(5).await;
    raw.send(Frame::Release { id: 1 });
    until(|| raw.result(2).is_some()).await;
    let result = raw.result(2).unwrap();
    assert!(result.error.is_some(), "{result:?}");
    gate.open(Json::Null);
    sleep_ms(10).await;
    assert!(!c.sent_to_worker().iter().any(|frame| matches!(frame, Frame::Hold(_))));
}

#[tokio::test]
async fn an_origin_disconnect_removes_its_relayed_holds_and_subscriptions_downstream() {
    let _serial = serial().await;
    let c = Cluster::new();
    let browser = c.browser(json!({"guest": "g"}), None);
    let counter = hold(&browser.host, "a");
    let _values = on_value(counter.state("count").unwrap().as_ref(), |_| {});
    call(&counter, "add", json!(1)).await.unwrap();
    assert_eq!(worker_link(&c.server).await.relayed_refs, 1);
    browser.up.close(None);
    until_worker_link(&c.server, |down| down.relayed_refs == 0).await;
    until(|| c.logged("close a") > 0).await;
}

#[tokio::test]
async fn two_browsers_share_one_worker_connection_and_one_leaving_releases_only_its_own_holds() {
    let _serial = serial().await;
    let c = Cluster::new();
    let one = c.browser(json!({"guest": "g1"}), None);
    let two = c.browser(json!({"guest": "g2"}), None);
    let a = hold(&one.host, "same");
    let b = hold(&two.host, "same");
    assert_eq!(call(&a, "add", json!(1)).await.unwrap(), json!(1));
    assert_eq!(call(&b, "add", json!(5)).await.unwrap(), json!(5));
    assert_eq!(c.workers.lock().unwrap().len(), 1);
    assert_eq!(c.principals.all(), vec![json!({"guest": "g1"}), json!({"guest": "g2"})]);
    one.up.close(None);
    until(|| c.logged("close same") == 1).await;
    assert_eq!(call(&b, "add", json!(1)).await.unwrap(), json!(6));
}

/// The adapter's view for version 1 consumers: `{text}` from the provider's `{body}`, every change as a reset.
struct TextView(ReadableRef);

fn text_of(value: &Json) -> Json {
    json!({"text": value["body"].clone()})
}

impl Readable for TextView {
    fn value(&self) -> ChordResult<Json> {
        Ok(text_of(&self.0.value()?))
    }

    fn subscribe(&self, listener: ValueListener) -> Unsubscribe {
        on_event(self.0.as_ref(), move |event| listener(&text_of(event.value())))
    }

    fn events(&self, listener: EventListener) -> Unsubscribe {
        on_event(self.0.as_ref(), move |event| {
            let value = text_of(event.value());
            listener(&match event {
                StateEvent::Snap { .. } => StateEvent::Snap { value },
                _ => StateEvent::Reset { value },
            })
        })
    }
}

fn doc(version: i64, field: &str) -> Contract {
    let main = VariantDef::new().state("view", object(&[req(field, string())]));
    let host = host_named("server").unwrap();
    remote("t.doc", &RemoteDef { version, input: object(&[]), host, variants: vec![("main".into(), main)] }).unwrap()
}

#[tokio::test]
async fn snapshots_through_an_accepts_adapter_are_validated_against_the_consumer_schema() {
    let _serial = serial().await;
    let server = named("server");
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let view = decl.state("view", object(&[req("body", string())]), json!({"body": "hi"}))?;
        Ok(Implementation::variant("main").state("view", view))
    };
    let adapter: Adapter = Arc::new(|current: &Implementation| {
        let Some(Member::State(view)) = current.get("view") else { panic!("no view member") };
        let text: ReadableRef = Arc::new(TextView(view.clone()));
        Implementation::variant("main").state("view", text)
    });
    let options = ServeOptions { accepts: Some(HashMap::from([(1, adapter)])), ..ServeOptions::default() };
    server.serve(&doc(2, "body"), factory, options).unwrap();
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    let browser = browser_on(&up, LinkOptions::default());
    let doc = browser.hold(&doc(1, "text"), json!({}), GetOptions::default()).unwrap();
    doc.ready().await.unwrap();
    assert_eq!(doc.state("view").unwrap().value().unwrap(), json!({"text": "hi"}));
}
