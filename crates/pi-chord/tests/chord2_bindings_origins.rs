//! Port of upstream `test/bindings.test.ts`, "origins of client holds": a relayed hold carries the id of its
//! client connection, the server's own holds carry none, and an origin a client sends is replaced.

mod chord2_support;

use std::collections::HashSet;
use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::{Frame, Hello, Hold, PROTOCOL_VERSION, PeerKind};
use pi_chord::host2::{
    AcceptOptions, Authorize, Contract, Declare, GetOptions, Host, Implementation, Link, LinkOptions, Readable,
    Received, RemoteDef, Resolve, Role, Route, ServeOptions, VariantDef, host_family, memory_link_pair, method, remote,
};
use pi_chord::{Json, json};

/// A link that records the frames it sends (upstream `CountingLink`, sending side).
struct Counting {
    inner: Memory,
    sent: Log<Frame>,
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
        self.inner.receive(handler);
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

fn e8_counter() -> Contract {
    let main = VariantDef::new().state("n", number()).method("add", method(number(), None));
    let host = host_family("session:", "session").unwrap();
    let input = object(&[req("session", string())]);
    remote("t.e8-counter", &RemoteDef { version: 1, input, host, variants: vec![("main".into(), main)] }).unwrap()
}

fn serve_e8_counter(host: &Host) {
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let n = decl.state("n", number(), json!(0))?;
        let cell = n.clone();
        Ok(Implementation::variant("main").state("n", n).method("add", move |delta, ctx| {
            let cell = cell.clone();
            async move {
                let next = cell.value()?.as_f64().unwrap_or_default() + delta.as_f64().unwrap_or_default();
                cell.replace(&ctx, json!(next)).map(|()| Json::Null)
            }
        }))
    };
    host.serve(&e8_counter(), factory, ServeOptions::default()).unwrap();
}

/// A server routing `session:` to one worker, spawned on first use; the link to it records what it sends.
struct Cluster {
    server: Host,
    worker: Arc<Mutex<Option<Host>>>,
    to_worker: Arc<Mutex<Option<Arc<Counting>>>>,
}

fn cluster() -> Cluster {
    let server = named("server");
    let worker: Arc<Mutex<Option<Host>>> = Arc::default();
    let to_worker: Arc<Mutex<Option<Arc<Counting>>>> = Arc::default();
    let (spawned, linked) = (worker.clone(), to_worker.clone());
    let resolve: Resolve = Arc::new(move |name: &str| {
        let mut linked = linked.lock().unwrap();
        let link = linked.get_or_insert_with(|| {
            let (down, up) = memory_link_pair();
            let created = child_on(name, &up, LinkOptions::default(), &Log::default());
            serve_e8_counter(&created);
            *spawned.lock().unwrap() = Some(created);
            Arc::new(Counting { inner: down, sent: Log::default() })
        });
        let link: Arc<dyn Link> = link.clone();
        Box::pin(ready(Ok(link)))
    });
    let authorize: Authorize = Arc::new(|_: &Json, _: &str| Box::pin(ready(true)));
    let principal = Arc::new(|name: &str| json!({"host": name}));
    server.route("session:", Route { authorize, resolve, principal });
    Cluster { server, worker, to_worker }
}

impl Cluster {
    /// A browser accepted as a guest, with its end of the link.
    fn browser(&self) -> (Host, Memory) {
        let (down, up) = memory_link_pair();
        self.server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
        (browser_on(&up, LinkOptions::default()), up)
    }

    /// The origins of the holds the worker serves for its parent (`None`: a relayed hold or no origin).
    async fn origins(&self) -> Vec<Option<String>> {
        let Some(worker) = self.worker.lock().unwrap().clone() else { return Vec::new() };
        let connections = worker.connections().await;
        let upstream = connections.into_iter().filter(|connection| connection.role == Role::Upstream);
        let served = upstream.flat_map(|connection| connection.served);
        served.map(|served| if served.relay { None } else { served.origin }).collect()
    }

    async fn known_origins(&self) -> Vec<String> {
        self.origins().await.into_iter().flatten().collect()
    }

    fn holds_to_worker(&self) -> Vec<Hold> {
        let sent = self.to_worker.lock().unwrap().as_ref().map(|link| link.sent.all()).unwrap_or_default();
        sent.into_iter().filter_map(|frame| if let Frame::Hold(hold) = frame { Some(*hold) } else { None }).collect()
    }
}

/// Awaits `work` for at most 5 s (upstream's test timeout).
async fn within<T>(step: &str, work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(5), work).await.unwrap_or_else(|_| panic!("{step} timed out"))
}

fn s1() -> Json {
    json!({"session": "S1"})
}

#[tokio::test]
async fn a_relayed_hold_carries_its_client_connection_id_and_the_server_own_holds_carry_none() {
    let _serial = serial().await;
    let c = cluster();
    let (one, one_up) = c.browser();
    let (two, _two_up) = c.browser();
    let first = one.hold(&e8_counter(), s1(), GetOptions::default()).unwrap();
    within("first", first.ready()).await.unwrap();
    let second = two.hold(&e8_counter(), s1(), GetOptions::default()).unwrap();
    within("second", second.ready()).await.unwrap();
    let own = c.server.hold(&e8_counter(), s1(), GetOptions::default()).unwrap();
    within("own", own.ready()).await.unwrap();
    let holds = c.holds_to_worker();
    assert_eq!(holds.len(), 3);
    let relayed: Vec<&String> = holds.iter().filter_map(|hold| hold.origin.as_ref()).collect();
    assert_eq!(relayed.len(), 2);
    assert_ne!(relayed[0], relayed[1], "two tabs, two origins");
    assert_eq!(holds.iter().filter(|hold| hold.origin.is_none()).count(), 1);

    // Disconnect is departure: the worker loses that origin at once; a reconnect holds under a new id.
    let before: HashSet<Option<String>> = c.origins().await.into_iter().collect();
    one_up.close(None);
    let mut left = 0;
    for _ in 0..500 {
        left = c.known_origins().await.len();
        if left == 1 {
            break;
        }
        sleep_ms(2).await;
    }
    assert_eq!(left, 1, "condition not reached");
    let (three, _three_up) = c.browser();
    let third = three.hold(&e8_counter(), s1(), GetOptions::default()).unwrap();
    within("third", third.ready()).await.unwrap();
    let after = c.known_origins().await;
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(|origin| !before.contains(&Some(origin.clone()))), "a new connection gets a new id");
    within("close", c.server.close()).await.unwrap();
}

#[tokio::test]
async fn an_origin_a_client_sends_is_ignored_and_the_server_stamps_its_own() {
    let _serial = serial().await;
    let c = cluster();
    let (down, raw) = memory_link_pair();
    c.server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    raw.receive(Arc::new(|_: Received| {}));
    let hello = Hello {
        protocol: Some(PROTOCOL_VERSION),
        name: "raw".into(),
        kind: PeerKind::Client,
        code_version: 0,
        acks: None,
    };
    raw.send(&Frame::Hello(hello));
    raw.send(&Frame::Hold(Box::new(Hold {
        id: 1,
        at: "session:S1".into(),
        token: e8_counter().description().clone(),
        input: s1(),
        principal: None,
        origin: Some("spoofed".into()),
        state: true,
        again: false,
    })));
    soon(|| !c.holds_to_worker().is_empty()).await;
    let hold = c.holds_to_worker().remove(0);
    assert!(hold.origin.is_some_and(|origin| origin != "spoofed"));
    c.server.close().await.unwrap();
}
