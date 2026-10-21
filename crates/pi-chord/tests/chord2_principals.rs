//! Port of upstream `test/principals.test.ts`, in-host part. Identities are untyped JSON in Rust, so the
//! narrowing checks are left out with the type errors; so are the link cases (a child's parent identity and
//! `connect` or `route` without one: phase 3) and `createHost` without `self` (a required field here).

mod chord2_support;

use std::future::ready;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chord2_support::*;
use pi_chord::host2::{
    Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation, LoadOptions, PrincipalMatch, RefStatus,
    RootOptions, ServeOptions, create_host, define_facet, method,
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
