//! Port of upstream `test/placement.test.ts`. Left out: the type errors for a family's input field (typo, number,
//! optional), which `remote` does not check at runtime.

mod chord2_support;

use std::collections::HashMap;
use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::object::Adapter;
use pi_chord::host2::wire::{Frame, Hello, Hold, PROTOCOL_VERSION, PeerKind};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, GetOptions, Host, HostOptions, Implementation, Link, LoadOptions,
    Placement, Received, RemoteDef, Route, ServeOptions, VariantDef, bind, create_host, define_facet, host_family,
    host_named, memory_link_pair, method, remote,
};
use pi_chord::{Json, json};

fn wheres() -> Vec<(String, VariantDef)> {
    vec![("main".into(), VariantDef::new().method("where", method(None, string())))]
}

/// Lives on `session:` + its input's `session`.
fn note() -> Contract {
    let input = object(&[req("session", string()), req("id", string())]);
    let host = host_family("session:", "session").unwrap();
    remote("t.note", &RemoteDef { version: 1, input, host, variants: wheres() }).unwrap()
}

/// Lives on `server`.
fn catalog() -> Contract {
    let host = host_named("server").unwrap();
    remote("t.catalog", &RemoteDef { version: 1, input: object(&[]), host, variants: wheres() }).unwrap()
}

fn at_place(place: String) -> Implementation {
    Implementation::variant("main").method("where", move |_, _| ready(Ok(json!(place.clone()))))
}

/// A host named `name` whose own identity is `"host"` and whose errors are dropped.
fn options(name: &str) -> HostOptions {
    HostOptions { principal: json!("host"), ..host_options(name, &Log::default()) }
}

fn plain(name: &str) -> Host {
    create_host(options(name))
}

/// A server with two session hosts as children; every host serves what it may and reports its name.
struct Placed {
    server: Host,
    s1: Host,
    s2: Host,
}

impl Placed {
    async fn close(&self) {
        self.s1.close().await.unwrap();
        self.s2.close().await.unwrap();
        self.server.close().await.unwrap();
    }
}

fn cluster() -> Placed {
    let server = plain("server");
    server.serve(&catalog(), |_, _, _| Ok(at_place("server".into())), ServeOptions::default()).unwrap();
    let links: Arc<Mutex<HashMap<String, Memory>>> = Arc::default();
    let child = |name: &str| {
        let (down, up) = memory_link_pair();
        links.lock().unwrap().insert(name.to_string(), down.clone());
        server.accept(down, AcceptOptions::host(name, json!("server-side"))).unwrap();
        let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up, ConnectOptions::new(json!("server"))));
        let host = create_host(HostOptions { upstream, ..options(name) });
        let host_name = name.to_string();
        let factory = move |input: &Json, _: &Json, _: &mut pi_chord::host2::Declare| {
            Ok(at_place(format!("{host_name} {}", input["id"].as_str().unwrap_or_default())))
        };
        host.serve(&note(), factory, ServeOptions::default()).unwrap();
        host
    };
    let (s1, s2) = (child("session:S1"), child("session:S2"));
    // Relays need a route (where authorization lives); it finds the connected child.
    let resolve: pi_chord::host2::Resolve = Arc::new(move |name: &str| {
        let link: Arc<dyn Link> = links.lock().unwrap()[name].clone();
        Box::pin(ready(Ok(link)))
    });
    let authorize: pi_chord::host2::Authorize = Arc::new(|_: &Json, _: &str| Box::pin(ready(true)));
    server.route("session:", Route { authorize, resolve, principal: Arc::new(|name: &str| json!(name)) });
    Placed { server, s1, s2 }
}

async fn where_of(host: &Host, contract: &Contract, input: Json) -> Json {
    call(&host.hold(contract, input, GetOptions::default()).unwrap(), "where", Json::Null).await.unwrap()
}

#[tokio::test]
async fn a_get_goes_where_the_contract_places_the_input_never_named_by_the_caller() {
    let _serial = serial().await;
    let placed = cluster();
    tick().await;
    let server = &placed.server;
    assert_eq!(where_of(server, &note(), json!({"session": "S1", "id": "a"})).await, json!("session:S1 a"));
    assert_eq!(where_of(server, &note(), json!({"session": "S2", "id": "b"})).await, json!("session:S2 b"));
    // From a session host: its own notes locally, the other session's through the server, the catalog upstream.
    assert_eq!(where_of(&placed.s1, &note(), json!({"session": "S1", "id": "c"})).await, json!("session:S1 c"));
    assert_eq!(where_of(&placed.s1, &catalog(), json!({})).await, json!("server"));
    placed.close().await;
}

#[tokio::test]
async fn serve_refuses_a_type_the_host_is_not_the_place_of_also_at_a_reload() {
    let _serial = serial().await;
    let host = plain("session:S1");
    let error = host.serve(&catalog(), |_, _, _| Ok(at_place(String::new())), ServeOptions::default()).unwrap_err();
    assert!(error.to_string().contains("t.catalog is declared for server, but session:S1 serves it"), "{error}");
    let server = plain("server");
    let error = server.serve(&note(), |_, _, _| Ok(at_place(String::new())), ServeOptions::default()).unwrap_err();
    assert!(error.to_string().contains("t.note is declared for session:<session>, but server serves it"), "{error}");
    let misplaced = define_facet("misplaced", |api| {
        api.serve(
            &note(),
            |_: &Json, _: &Json, _: &mut pi_chord::host2::Declare| Ok(at_place(String::new())),
            ServeOptions::default(),
        )
    });
    let error = server.reload(vec![misplaced.into()], Default::default()).await.unwrap_err();
    assert!(error.to_string().contains("declared for session:<session>, but server serves it"), "{error}");
    host.close().await.unwrap();
    server.close().await.unwrap();
}

/// What a raw client gets for a hold of a note at `target`: the first `shape` or `gone` frame.
async fn holds(server: &Host, target: &str, input: Json) -> Option<Frame> {
    let (down, raw) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!("guest"))).unwrap();
    let frames: Log<Frame> = Log::default();
    let seen = frames.clone();
    raw.receive(Arc::new(move |received| {
        if let Received::Frame(frame) = received {
            seen.push(frame);
        }
    }));
    raw.send(&Frame::Hello(Hello {
        protocol: Some(PROTOCOL_VERSION),
        name: "raw".into(),
        kind: PeerKind::Client,
        code_version: 0,
        acks: None,
    }));
    let token = note().description().clone();
    let hold =
        Hold { id: 1, at: target.into(), token, input, principal: None, origin: None, state: true, again: false };
    raw.send(&Frame::Hold(Box::new(hold)));
    let found = || frames.all().into_iter().find(|frame| matches!(frame, Frame::Shape(_) | Frame::Gone { .. }));
    for _ in 0..50 {
        if found().is_some() {
            break;
        }
        sleep_ms(5).await;
    }
    found()
}

#[tokio::test]
async fn a_host_refuses_a_hold_its_own_contract_places_elsewhere_and_a_relay_one_whose_target_disagrees() {
    let _serial = serial().await;
    let placed = cluster();
    tick().await;
    // A client picks session:S2 for a note of S1: the relay refuses, nothing reaches S2.
    let relayed = holds(&placed.server, "session:S2", json!({"session": "S1", "id": "x"})).await;
    let Some(Frame::Gone { error, .. }) = relayed else { panic!("not gone: {relayed:?}") };
    let message = error.message;
    assert!(message.contains("placement") || message.contains("does not live on session:S2"), "{message}");
    // The right target is served.
    let served = holds(&placed.server, "session:S1", json!({"session": "S1", "id": "x"})).await;
    assert!(matches!(served, Some(Frame::Shape(_))), "{served:?}");
    // Addressed to the server itself, a note is refused by the server's own contract check.
    let direct = holds(&placed.server, "server", json!({"session": "S1", "id": "x"})).await;
    assert!(matches!(direct, Some(Frame::Gone { .. })), "{direct:?}");
    placed.close().await;
}

#[tokio::test]
async fn the_host_comes_from_the_normalized_input_unknown_fields_and_defaults_do_not_change_it() {
    let _serial = serial().await;
    let input = object(&[req("session", with(string(), serde_json::json!({"default": "S1"})))]);
    let host = host_family("session:", "session").unwrap();
    let doc = remote("t.doc-default", &RemoteDef { version: 1, input, host, variants: wheres() }).unwrap();
    let placed = cluster();
    placed.s1.serve(&doc, |_, _, _| Ok(at_place("session:S1".into())), ServeOptions::default()).unwrap();
    tick().await;
    // An untyped caller leaves the field out: the default places it.
    assert_eq!(where_of(&placed.server, &doc, json!({})).await, json!("session:S1"));
    let error = placed.server.hold(&note(), json!({"session": "", "id": "x"}), GetOptions::default()).unwrap_err();
    assert!(error.to_string().contains("input field session names no host"), "{error}");
    placed.close().await;
}

fn moved(version: i64, place: &str) -> Contract {
    let variants = vec![("main".into(), VariantDef::new().method("ping", method(None, None)))];
    let def = RemoteDef { version, input: object(&[]), host: host_named(place).unwrap(), variants };
    remote("t.moved", &def).unwrap()
}

fn identity_accepts() -> HashMap<i64, Adapter> {
    let identity: Adapter = Arc::new(|current: &Implementation| current.clone());
    HashMap::from([(1, identity)])
}

#[tokio::test]
async fn placement_is_part_of_the_contract_a_moved_type_is_never_bridged_by_an_adapter() {
    let _serial = serial().await;
    assert_ne!(moved(1, "server").fingerprint(), moved(1, "session:S1").fingerprint());
    let host = plain("server");
    let facet = |version: i64, place: &'static str| {
        define_facet("moved", move |api| {
            let implementation = |_: &Json, _: &Json, _: &mut pi_chord::host2::Declare| {
                Ok(Implementation::variant("main").method("ping", |_, _| ready(Ok(Json::Null))))
            };
            let options = ServeOptions { accepts: Some(identity_accepts()), ..ServeOptions::default() };
            api.serve(&moved(version, place), implementation, options)
        })
    };
    host.install(&facet(1, "server"), LoadOptions::default()).await.unwrap();
    host.get(&moved(1, "server"), json!({}), GetOptions::default()).await.unwrap();
    // Same version, other placement: a forgotten bump.
    let error = host.reload(vec![facet(1, "session:S1").into()], Default::default()).await.unwrap_err();
    let message = error.to_string();
    assert!(message.contains("declared for session:S1") || message.contains("without a version bump"), "{error}");
    // A new version that `accepts` version 1 but lives elsewhere: no adapter makes version 1 consumers bind.
    let accepts = identity_accepts();
    let error = bind(&moved(2, "session:S1"), Some(&accepts), &moved(1, "server")).err().expect("a mismatch");
    assert!(is(&error, "contract_mismatch") && error.to_string().contains("lives on session:S1"), "{error}");
    host.close().await.unwrap();
}

#[tokio::test]
async fn types_a_family_names_an_input_field_and_remote_contracts_need_a_host() {
    let _serial = serial().await;
    let input = object(&[req("session", string()), req("count", number()), opt("note", string())]);
    let variants = vec![("main".to_string(), VariantDef::new())];
    let host = host_family("session:", "session").unwrap();
    let ok = RemoteDef { version: 1, input: input.clone(), host, variants: variants.clone() };
    remote("t.ok", &ok).unwrap();
    // Rust has no remote contract without a host; an empty one stands for it.
    let nowhere = RemoteDef { version: 1, input, host: Placement::Named { name: String::new() }, variants };
    let error = remote("t.nowhere", &nowhere).unwrap_err();
    assert!(error.to_string().contains("needs `host`"), "{error}");
    let error = host_family("", "session").unwrap_err();
    assert!(error.to_string().contains("prefix"), "{error}");
}
