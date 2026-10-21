//! Port of upstream `test/principals.test.ts`. Identities are untyped JSON in Rust, so the narrowing checks are
//! left out with the type errors; so is the case of a host, a parent link or a route without an identity (each one
//! is a required field here).

mod chord2_support;

use std::future::ready;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chord2_support::*;
use pi_chord::host2::wire::PeerKind;
use pi_chord::host2::{
    AcceptOptions, Authorize, ConnectOptions, Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation,
    Link, LoadOptions, PrincipalMatch, RefStatus, RemoteDef, Resolve, RootOptions, Route, ServeOptions, VariantDef,
    create_host, define_facet, host_named, memory_link_pair, method, remote,
};
use pi_chord::{Json, json};

fn principal_key(principal: &Json) -> String {
    let kind = principal.get("kind").and_then(Json::as_str).unwrap_or_default();
    match principal.get("id").and_then(Json::as_str) {
        Some(id) if kind == "guest" => format!("guest:{id}"),
        _ => kind.to_string(),
    }
}

fn host() -> Host {
    create_host(HostOptions {
        principal: json!({"kind": "host"}),
        principal_key: Some(Arc::new(principal_key)),
        ..HostOptions::default()
    })
}

fn who() -> Contract {
    local_type("t.who", object(&[]), vec![("who", method(None, string()).into())])
}

fn kind(principal: &Json) -> String {
    principal.get("kind").and_then(Json::as_str).unwrap_or_default().to_string()
}

#[tokio::test]
async fn factories_methods_and_contexts_see_the_identity() {
    let _serial = serial().await;
    let host = host();
    let tasks: Log = Log::default();
    let tasked = tasks.clone();
    let facet = define_facet("who", move |api| {
        let tasked = tasked.clone();
        let factory = move |_: &Json, principal: &Json, decl: &mut Declare| {
            let created = match principal.get("id").and_then(Json::as_str) {
                Some(id) => format!("guest {id}"),
                None => kind(principal),
            };
            let tasked = tasked.clone();
            decl.task(move |ctx| {
                tasked.push(kind(ctx.principal()));
                ready(Ok(()))
            });
            Ok(Implementation::new().method("who", move |_, ctx| {
                ready(Ok(json!(format!("{created}, called by {}", kind(ctx.principal())))))
            }))
        };
        api.serve(&who(), factory, ServeOptions::default())
    });
    host.install(&facet, LoadOptions::default()).await.unwrap();
    let guest_identity = json!({"kind": "guest", "id": "g1"});
    let guest = host.hold(&who(), json!({}), GetOptions::principal(guest_identity.clone())).unwrap();
    let run = move |ctx: Ctx| async move { guest.call_json("who", Json::Null, Some(&ctx)).await };
    let options = RootOptions { principal: Some(guest_identity), ..RootOptions::default() };
    assert_eq!(host.root(run, options).await.unwrap(), json!("guest g1, called by host"));
    until(|| tasks.len() == 1).await;
    assert_eq!(tasks.all(), ["host"]);
    host.invalidate_principal(PrincipalMatch::Pattern(json!({"kind": "guest", "name": "g1"}))).await.unwrap();
    host.close().await.unwrap();
}

#[tokio::test]
async fn the_host_identity_is_recognized_by_key_and_an_equal_object_reaches_the_same_objects() {
    let _serial = serial().await;
    let host = host();
    let created = Arc::new(AtomicUsize::new(0));
    let counted = created.clone();
    let factory = move |_: &Json, _: &Json, _: &mut Declare| {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(Implementation::new().method("who", |_, _| ready(Ok(json!("one")))))
    };
    host.serve(&who(), factory, ServeOptions::default()).unwrap();
    let own = host.hold(&who(), json!({}), GetOptions::default()).unwrap();
    let equal = host.hold(&who(), json!({}), GetOptions::principal(json!({"kind": "host"}))).unwrap();
    own.ready().await.unwrap();
    equal.ready().await.unwrap();
    assert_eq!(created.load(Ordering::SeqCst), 1);
    // Never disposed as a matching identity: it is the host's own.
    host.invalidate_principal(PrincipalMatch::Pattern(json!({"kind": "host"}))).await.unwrap();
    assert_eq!(own.status(), RefStatus::Live);
    host.close().await.unwrap();
}

/// A host named `name` with the app's identity key, whose parent is reached over `up` (if any).
fn linked(name: &str, kind: PeerKind, up: Option<Arc<dyn Link>>) -> Host {
    let upstream = up.map(|up| (up, ConnectOptions::new(json!({"kind": "server"}))));
    create_host(HostOptions {
        name: Some(name.into()),
        principal: json!({"kind": "host"}),
        principal_key: Some(Arc::new(principal_key)),
        kind,
        upstream,
        ..HostOptions::default()
    })
}

#[tokio::test]
async fn a_child_parent_acts_as_the_identity_the_child_gave_connect_and_relayed_callers_keep_theirs() {
    let _serial = serial().await;
    let main = VariantDef::new().method("who", method(None, string()));
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named("worker").unwrap(),
        variants: vec![("main".into(), main)],
    };
    let echo = remote("t.echo", &def).unwrap();
    let server = linked("server", PeerKind::Host, None);
    let (down, up) = memory_link_pair();
    server.accept(down.clone(), AcceptOptions::host("worker", json!({"kind": "host"}))).unwrap();
    let worker = linked("worker", PeerKind::Host, Some(up));
    let factory = |_: &Json, principal: &Json, _: &mut Declare| {
        let key = principal_key(principal);
        Ok(Implementation::variant("main").method("who", move |_, _| ready(Ok(json!(key.clone())))))
    };
    worker.serve(&echo, factory, ServeOptions::default()).unwrap();
    let (browser_down, browser_up) = memory_link_pair();
    server.accept(browser_down, AcceptOptions::client(json!({"kind": "guest", "id": "g1"}))).unwrap();
    let browser = linked("browser", PeerKind::Client, Some(browser_up));
    let who = |host: &Host| {
        let echo = host.hold(&echo, json!({}), GetOptions::default()).unwrap();
        async move { echo.call_json("who", Json::Null, None).await.unwrap() }
    };
    assert_eq!(who(&server).await, json!("server"));
    let authorize: Authorize = Arc::new(|_: &Json, _: &str| Box::pin(ready(true)));
    let resolve: Resolve = Arc::new(move |_: &str| {
        let link: Arc<dyn Link> = down.clone();
        Box::pin(ready(Ok(link)))
    });
    let principal = Arc::new(|_: &str| json!({"kind": "host"}));
    server.route("worker", Route { authorize, resolve, principal });
    assert_eq!(who(&browser).await, json!("guest:g1"));
    browser.close().await.unwrap();
    worker.close().await.unwrap();
    server.close().await.unwrap();
}
