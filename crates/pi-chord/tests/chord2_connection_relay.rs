//! Port of upstream `test/link.test.ts`, "links: relay, stamping, lifetime": a server routing `session:` hosts to
//! workers it spawns, browsers connected as clients, raw links that try to forward identities.

mod chord2_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::memory_link_pair;
use pi_chord::host2::wire::{Call, Frame, Hello, Hold, PROTOCOL_VERSION, PeerKind};
use pi_chord::host2::{
    AcceptOptions, GetOptions, Link, LinkOptions, PrincipalMatch, Received, RefStatus, Route, create_host, on_event,
    on_value,
};
use pi_chord::{Json, json};

fn session(name: &str) -> Json {
    json!({"session": "S1", "name": name})
}

/// Frames a raw link receives, as a log.
fn record(link: &Memory) -> Log<Frame> {
    let frames: Log<Frame> = Log::default();
    let seen = frames.clone();
    link.receive(Arc::new(move |received| {
        if let Received::Frame(frame) = received {
            seen.push(frame);
        }
    }));
    frames
}

/// The value of the first `result` frame, once one arrived.
async fn first_result(frames: &Log<Frame>) -> Option<Json> {
    for _ in 0..50 {
        let found = frames.all().into_iter().find_map(|frame| match frame {
            Frame::Result(result) => Some(result.value.map(|value| pi_chord::host2::unpack(&value).unwrap())),
            _ => None,
        });
        if let Some(value) = found {
            return value;
        }
        sleep_ms(5).await;
    }
    panic!("no result frame");
}

fn raw_hold(id: u64, at: &str, contract: &pi_chord::host2::Contract, input: Json) -> Frame {
    Frame::Hold(Box::new(Hold {
        id,
        at: at.into(),
        token: contract.description().clone(),
        input,
        principal: Some(json!({"owner": true})),
        origin: None,
        state: true,
        again: false,
    }))
}

fn raw_call(id: u64, hold: u64, member: &str) -> Frame {
    Frame::Call(Call { id, hold, member: member.into(), args: None, root: None })
}

#[tokio::test]
async fn relays_browser_server_worker_and_the_worker_sees_the_browser_identity() {
    let _serial = serial().await;
    let cluster = Cluster::new(LinkOptions::default());
    let guest = json!({"guest": "g1"});
    let browser = cluster.browser(guest.clone(), None);
    let counter = browser.host.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    assert_eq!(call(&counter, "add", json!(3)).await.unwrap(), json!(3));
    assert_eq!(call(&counter, "who", Json::Null).await.unwrap(), guest);
    let values: Log<Json> = Log::default();
    let seen = values.clone();
    let _values = on_value(counter.state("count").unwrap().as_ref(), move |value| seen.push(value.clone()));
    call(&counter, "add", json!(1)).await.unwrap();
    until(|| values.all().last() == Some(&json!({"n": 4}))).await;
}

#[tokio::test]
async fn ignores_payload_identities_from_clients_and_from_child_hosts() {
    let _serial = serial().await;
    let cluster = Cluster::new(LinkOptions::default());
    let guest = json!({"guest": "g1"});
    let (down, raw) = memory_link_pair();
    cluster.server.accept(down, AcceptOptions::client(guest.clone())).unwrap();
    let frames = record(&raw);
    let hello = Hello {
        protocol: Some(PROTOCOL_VERSION),
        name: "raw".into(),
        kind: PeerKind::Client,
        code_version: 0,
        acks: None,
    };
    raw.send(&Frame::Hello(hello));
    raw.send(&raw_hold(1, "session:S1", &session_counter(), json!({"session": "S1", "name": "f"})));
    raw.send(&raw_call(2, 1, "who"));
    assert_eq!(first_result(&frames).await, Some(guest));

    // A child host cannot forward identities upward either: the server stamps it.
    let pair = server_with_worker("w9");
    serve_counter(&pair.server, &Log::default());
    // The worker connects in its link task: replace its receiver once that is done.
    connected(&pair.worker, 1).await;
    let child_frames = record(&pair.up);
    pair.up.send(&raw_hold(7, "server", &counter(), json!({"name": "f"})));
    pair.up.send(&raw_call(8, 7, "who"));
    assert_eq!(first_result(&child_frames).await, Some(json!({"worker": "w9"})));
}

#[tokio::test]
async fn a_route_that_returns_an_already_closed_link_does_not_poison_the_host_name() {
    let _serial = serial().await;
    let server = named("server");
    let attempt = Arc::new(AtomicUsize::new(0));
    let attempts = attempt.clone();
    let resolve: pi_chord::host2::Resolve = Arc::new(move |name: &str| {
        let (down, up) = memory_link_pair();
        let worker = child_on(name, &up, LinkOptions::default(), &Log::default());
        serve_counter(&worker, &Log::default());
        // The first worker dies before the server wraps its link.
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            down.close(Some("worker exited"));
        }
        let down: Arc<dyn Link> = down;
        Box::pin(std::future::ready(Ok(down)))
    });
    let route = Route {
        authorize: Arc::new(|_: &Json, _: &str| Box::pin(std::future::ready(true))),
        resolve,
        principal: Arc::new(|name: &str| json!({"host": name})),
    };
    server.route("session:", route);
    let first = server.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    let error = call(&first, "add", json!(1)).await.unwrap_err();
    assert!(error.to_string().contains("closed"), "{error}");
    // The next call resolves the route again and reaches the new worker.
    let second = server.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    assert_eq!(call(&second, "add", json!(2)).await.unwrap(), json!(2));
    assert_eq!(attempt.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authorizes_before_resolving_and_again_after() {
    let _serial = serial().await;
    let cluster = Cluster::new(LinkOptions::default());
    cluster.set_allow(|who, _| who["guest"] != json!("blocked"));
    let blocked = cluster.browser(json!({"guest": "blocked"}), None);
    let held = blocked.host.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    let error = held.ready().await.unwrap_err();
    assert!(is(&error, "forbidden"), "{error}");
    assert_eq!(cluster.worker_count(), 0);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    cluster.set_allow(move |_, _| counted.fetch_add(1, Ordering::SeqCst) == 0);
    let flaky = cluster.browser(json!({"guest": "flaky"}), None);
    let held = flaky.host.hold(&session_counter(), json!({"session": "S2", "name": "a"}), GetOptions::default());
    let error = held.unwrap().ready().await.unwrap_err();
    assert!(is(&error, "forbidden"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_browser_disconnect_releases_exactly_its_holds_and_the_worker_link_stays_up() {
    let _serial = serial().await;
    let cluster = Cluster::new(LinkOptions::default());
    let one = cluster.browser(json!({"guest": "g1"}), None);
    let two = cluster.browser(json!({"guest": "g2"}), None);
    let first = one.host.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    call(&first, "add", json!(1)).await.unwrap();
    let other = two.host.hold(&session_counter(), session("b"), GetOptions::default()).unwrap();
    call(&other, "add", json!(1)).await.unwrap();
    one.up().close(None);
    until(|| cluster.log.all().contains(&"close a".to_string())).await;
    assert!(!cluster.log.all().contains(&"close b".to_string()));
    assert_eq!(call(&other, "add", json!(1)).await.unwrap(), json!(2));
}

#[tokio::test]
async fn a_dead_link_without_pongs_is_closed_and_everything_held_through_it_is_released() {
    let _serial = serial().await;
    // Upstream waits 40 ms for a pong; 200 ms keeps a loaded machine from timing out the live links too.
    let cluster = Cluster::new(pings(10, 200));
    let browser = cluster.browser(json!({"guest": "g1"}), None);
    let counter = browser.host.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    call(&counter, "add", json!(1)).await.unwrap();
    browser.up().sever();
    for _ in 0..300 {
        if cluster.log.all().contains(&"close a".to_string()) {
            break;
        }
        sleep_ms(10).await;
    }
    assert!(cluster.log.all().contains(&"close a".to_string()));
}

#[tokio::test]
async fn checks_contract_tokens_across_links() {
    let _serial = serial().await;
    let variants = vec![(
        "main".to_string(),
        pi_chord::host2::VariantDef::new().method("inc", pi_chord::host2::method(None, number())),
    )];
    let def = pi_chord::host2::RemoteDef {
        version: 2,
        input: object(&[req("name", string())]),
        host: pi_chord::host2::host_named("server").unwrap(),
        variants,
    };
    let counter_v2 = pi_chord::host2::remote("t.counter", &def).unwrap();
    let pair = server_with_worker("session:S1");
    let factory = |_: &Json, _: &Json, _: &mut pi_chord::host2::Declare| {
        Ok(pi_chord::host2::Implementation::variant("main").method("inc", |_, _| std::future::ready(Ok(json!(1)))))
    };
    pair.server.serve(&counter_v2, factory, pi_chord::host2::ServeOptions::default()).unwrap();
    let held = pair.worker.hold(&counter(), json!({"name": "x"}), GetOptions::default()).unwrap();
    let error = held.ready().await.unwrap_err();
    assert!(is(&error, "contract_mismatch"), "{error}");
    let (down, up) = memory_link_pair();
    pair.server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    let browser = browser_on(&up, LinkOptions::default());
    let held = browser.hold(&counter(), json!({"name": "x"}), GetOptions::default()).unwrap();
    let error = held.ready().await.unwrap_err();
    assert!(is(&error, "upgrade_required"), "{error}");
}

#[tokio::test]
async fn invalidate_principal_restamps_the_link_and_propagates_to_workers() {
    let _serial = serial().await;
    let cluster = Cluster::new(LinkOptions::default());
    let restamp: pi_chord::host2::Restamp = Arc::new(|who: &Json| {
        let mut next = who.clone();
        next.set("revision", 2).unwrap();
        Some(next)
    });
    let browser = cluster.browser(json!({"guest": "g1", "revision": 1}), Some(restamp));
    let counter = browser.host.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    call(&counter, "add", json!(1)).await.unwrap();
    cluster.server.invalidate_principal(PrincipalMatch::Pattern(json!({"guest": "g1"}))).await.unwrap();
    // The relay under the old identity is closed at once; the worker disposes its object after.
    until(|| counter.status() == RefStatus::Gone).await;
    until(|| cluster.log.all().contains(&"close a".to_string())).await;
    let again = browser.host.hold(&session_counter(), session("a"), GetOptions::default()).unwrap();
    assert_eq!(call(&again, "who", Json::Null).await.unwrap(), json!({"guest": "g1", "revision": 2}));
}

#[tokio::test]
async fn after_a_reconnect_state_subscribers_get_a_reset_and_calls_work_again() {
    let _serial = serial().await;
    let cluster = Cluster::new(LinkOptions::default());
    let browser = cluster.browser(json!({"guest": "g1"}), None);
    let counter = browser.host.hold(&session_counter(), session("r"), GetOptions::default()).unwrap();
    let events: Log = Log::default();
    let kinds = events.clone();
    let count = counter.state("count").unwrap();
    let _events = on_event(count.as_ref(), move |event| kinds.push(event.kind()));
    call(&counter, "add", json!(2)).await.unwrap();
    browser.up().close(None);
    until(|| counter.status() == RefStatus::Loading).await;
    browser.reconnect();
    assert_eq!(call(&counter, "add", json!(1)).await.unwrap(), json!(1));
    // The old hold was released with the link, so the worker made a fresh object; the reset carries its value.
    until(|| count.value().ok() == Some(json!({"n": 1}))).await;
    // The first hydration is the listener's snapshot; the re-hold's snapshot is a reset.
    until(|| events.all().contains(&"reset".to_string())).await;
    assert_eq!(events.all()[0], "snap");
}

#[tokio::test]
async fn a_host_created_without_links_has_no_connections() {
    let _serial = serial().await;
    let host = create_host(host_options("lonely", &Log::default()));
    assert!(host.connections().await.is_empty());
}
