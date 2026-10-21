//! Port of upstream `test/host.test.ts`, part 2: lifetime "host", identities and variants, registries and
//! composition. Left out: the "unknown lifetime" refusal (`Lifetime` is an enum).

mod chord2_support;

use std::future::ready;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chord2_support::*;
use pi_chord::host2::{
    Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation, Lifetime, PrincipalMatch, ReadableRef,
    RefStatus, RegistryView, RemoteDef, RootOptions, ServeOptions, Shape, StateEvent, VariantDef, create_host,
    define_registry, forbidden, host_named, method, on_event, on_value, opaque, remote,
};
use pi_chord::{Json, json};
use tokio::sync::oneshot;

type Factory = fn(&Json, &Json, &mut Declare) -> pi_chord::ChordResult<Implementation>;

fn hold(host: &Host, contract: &Contract, input: Json) -> pi_chord::host2::Ref {
    host.hold(contract, input, GetOptions::default()).unwrap()
}

fn empty(name: &str) -> Contract {
    local_type(name, object(&[]), vec![])
}

/// A host whose identities are keyed by `key` and whose errors are dropped.
fn keyed_host(key: impl Fn(&Json) -> String + Send + Sync + 'static) -> Host {
    create_host(HostOptions {
        principal: json!({"kind": "host"}),
        principal_key: Some(Arc::new(key)),
        on_error: Some(Arc::new(|_| {})),
        ..HostOptions::default()
    })
}

fn store() -> Contract {
    let scope = with(string(), serde_json::json!({"default": "all"}));
    local_type("t.store", object(&[opt("scope", scope)]), vec![("seen", opaque("t.seen@1").unwrap().into())])
}

#[tokio::test]
async fn serve_refuses_a_lifetime_host_the_type_cannot_have() {
    let _serial = serial().await;
    let host = quiet();
    let main = vec![("main".to_string(), VariantDef::new())];
    let def = RemoteDef { version: 1, input: object(&[]), host: host_named("host").unwrap(), variants: main };
    let remote_store = remote("t.remote-store", &def).unwrap();
    let keyed = local_type("t.keyed", object(&[req("id", string())]), vec![]);
    let main: Factory = |_, _, _| Ok(Implementation::variant("main"));
    let plain: Factory = |_, _, _| Ok(Implementation::new());
    let seen: Factory = |_, _, _| Ok(Implementation::new().opaque("seen", Json::Null));
    let lingering = ServeOptions { lifetime: Lifetime::Host, linger: Some(Duration::from_millis(10)), accepts: None };
    let refused = [
        (host.serve(&remote_store, main, ServeOptions::host()), "needs a local type"),
        (host.serve(&keyed, plain, ServeOptions::host()), "input that accepts {}"),
        (host.serve(&store(), seen, lingering), "does not linger"),
    ];
    for (result, message) in refused {
        let error = result.unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
    }
    // Allowed: a local type whose input has only optional fields, and any type with the default lifetime.
    let optional = local_type("t.optional", object(&[opt("id", string())]), vec![]);
    host.serve(&optional, plain, ServeOptions::host()).unwrap();
    host.serve(&keyed, plain, ServeOptions::linger(Duration::from_millis(10))).unwrap();
    host.serve(&remote_store, main, ServeOptions::linger(Duration::from_millis(10))).unwrap();
}

#[tokio::test]
async fn every_get_reaches_the_one_object_created_at_serve_for_the_host() {
    let _serial = serial().await;
    let host = keyed_host(|principal| principal.to_string());
    let created: Log<Json> = Log::default();
    let recorded = created.clone();
    let factory = move |input: &Json, principal: &Json, _: &mut Declare| {
        recorded.push(json!({"input": input, "principal": principal}));
        Ok(Implementation::new().opaque("seen", principal.clone()))
    };
    host.serve(&store(), factory, ServeOptions::host()).unwrap();
    host.ready().await.unwrap();
    assert_eq!(created.len(), 1, "created when served, before anyone asked");
    let guest = host.hold(&store(), json!({}), GetOptions::principal(json!({"guest": "g"}))).unwrap();
    let defaults = hold(&host, &store(), json!({"scope": "all"}));
    guest.ready().await.unwrap();
    defaults.ready().await.unwrap();
    assert_eq!(created.len(), 1, "the same object for every identity and the default input");
    assert_eq!(guest.opaque_as::<Json>("seen").unwrap(), Some(created.all()[0]["principal"].clone()));
    let error = host.hold(&store(), json!({"scope": "other"}), GetOptions::default()).unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    // References do not change its lifetime: released by everyone else, it stays until the host closes.
    guest.release();
    defaults.release();
    tick().await;
    let again = hold(&host, &store(), json!({}));
    again.ready().await.unwrap();
    assert_eq!(created.len(), 1);
    host.close().await.unwrap();
    assert_eq!(again.status(), RefStatus::Gone);
}

fn doc() -> Contract {
    let view = || object(&[req("text", string())]);
    let edit = method(object(&[req("text", string())]), object(&[req("ok", boolean())]));
    let variants = vec![
        ("viewer".to_string(), VariantDef::new().state("view", view())),
        ("editor".to_string(), VariantDef::new().state("view", view()).method("edit", edit)),
    ];
    let input = object(&[req("id", string())]);
    remote("t.doc", &RemoteDef { version: 1, input, host: host_named("host").unwrap(), variants }).unwrap()
}

fn serve_doc(host: &Host, principals: &Log<Json>) {
    let principals = principals.clone();
    let factory = move |_: &Json, principal: &Json, decl: &mut Declare| {
        principals.push(principal.clone());
        let view = decl.state("view", object(&[req("text", string())]), json!({"text": ""}))?;
        if principal.get("role").and_then(Json::as_str) != Some("editor") {
            return Ok(Implementation::variant("viewer").state("view", view));
        }
        Ok(Implementation::variant("editor").state("view", view.clone()).method("edit", move |args, ctx| {
            let text = args["text"].clone();
            let result = view.replace(&ctx, json!({"text": text}));
            let ok = if text.as_str() == Some("bad") { json!({"ok": "no"}) } else { json!({"ok": true}) };
            ready(result.map(|()| ok))
        }))
    };
    host.serve(&doc(), factory, ServeOptions::default()).unwrap();
}

#[tokio::test]
async fn keeps_objects_per_principal_key_and_enforces_variants() {
    let _serial = serial().await;
    let host = keyed_host(|principal| principal.get("user").and_then(Json::as_str).unwrap_or_default().to_string());
    let principals = Log::default();
    serve_doc(&host, &principals);
    let as_user = |user: &str, role: &str| GetOptions::principal(json!({"user": user, "role": role}));
    let viewer = host.hold(&doc(), json!({"id": "1"}), as_user("v", "viewer")).unwrap();
    let editor = host.hold(&doc(), json!({"id": "1"}), as_user("e", "editor")).unwrap();
    viewer.ready().await.unwrap();
    editor.ready().await.unwrap();
    assert_eq!(principals.len(), 2);
    assert_eq!(viewer.variant().as_deref(), Some("viewer"));
    assert_eq!(viewer.shape().unwrap().members, ["view"]);
    let error = viewer.call("edit", Some(json!({"text": "x"})), None).await.unwrap_err();
    assert!(is(&error, "not_in_variant"), "{error}");
    assert_eq!(editor.variant().as_deref(), Some("editor"));
    assert_eq!(editor.call_json("edit", json!({"text": "x"}), None).await.unwrap(), json!({"ok": true}));
    assert_eq!(editor.state("view").unwrap().value().unwrap(), json!({"text": "x"}));
    assert_eq!(viewer.state("view").unwrap().value().unwrap(), json!({"text": ""}));
}

#[tokio::test]
async fn validates_remote_args_and_results_at_the_provider_boundary() {
    let _serial = serial().await;
    let host = quiet();
    serve_doc(&host, &Log::default());
    let editor = host.get(&doc(), json!({"id": "2"}), GetOptions::principal(json!({"role": "editor"})));
    let editor = editor.await.unwrap();
    assert_eq!(editor.variant().as_deref(), Some("editor"));
    let error = editor.call("edit", Some(json!({"text": 1})), None).await.unwrap_err();
    assert!(is(&error, "invalid") && error.reason() == Some("args"), "{error}");
    let error = editor.call("edit", Some(json!({"text": "bad"})), None).await.unwrap_err();
    assert!(is(&error, "invalid") && error.reason() == Some("result"), "{error}");
}

#[tokio::test]
async fn forced_disposal_makes_references_gone_and_aborts_in_flight_calls() {
    let _serial = serial().await;
    let host = quiet();
    let seen_abort = Arc::new(AtomicBool::new(false));
    let slow = local_type("t.inflight", object(&[]), vec![("wait", method(None, None).into())]);
    let seen = seen_abort.clone();
    let factory = move |_: &Json, _: &Json, _: &mut Declare| {
        let seen = seen.clone();
        Ok(Implementation::new().method("wait", move |_, ctx| {
            let (sender, receiver) = oneshot::channel::<()>();
            let seen = seen.clone();
            ctx.signal().add_listener(move |_| {
                seen.store(true, Ordering::SeqCst);
                let _ = sender.send(());
            });
            async move {
                let _ = receiver.await;
                Ok(Json::Null)
            }
        }))
    };
    host.serve(&slow, factory, ServeOptions::default()).unwrap();
    let as_u = || GetOptions::principal(json!({"user": "u"}));
    let reference = host.hold(&slow, json!({}), as_u()).unwrap();
    reference.ready().await.unwrap();
    let call = tokio::spawn(reference.call("wait", None, None));
    tick().await;
    let matching = PrincipalMatch::predicate(|principal| principal.get("user").and_then(Json::as_str) == Some("u"));
    host.invalidate_principal(matching).await.unwrap();
    assert_eq!(reference.status(), RefStatus::Gone);
    call.await.unwrap().unwrap();
    assert!(seen_abort.load(Ordering::SeqCst));
    let error = reference.call("wait", None, None).await.unwrap_err();
    assert!(is(&error, "gone"), "{error}");
    let fresh = host.hold(&slow, json!({}), as_u()).unwrap();
    fresh.ready().await.unwrap();
    assert_eq!(fresh.status(), RefStatus::Live);
}

#[tokio::test]
async fn state_members_are_late_bound_readables() {
    let _serial = serial().await;
    let host = quiet();
    serve_doc(&host, &Log::default());
    let editor = host.hold(&doc(), json!({"id": "3"}), GetOptions::principal(json!({"role": "editor"}))).unwrap();
    let (shapes, events, values): (Log<Json>, Log<Json>, Log<Json>) = Default::default();
    let shaped = shapes.clone();
    let _shape = editor.on_shape(Arc::new(move |shape: &Shape| shaped.push(json!(shape.variant))));
    let view = editor.state("view").unwrap();
    let (evented, valued) = (events.clone(), values.clone());
    let _events = on_event(&*view, move |event| {
        evented.push(match event {
            StateEvent::Ops { ops, .. } => json!(["ops", ops.iter().map(|op| op.to_json()).collect::<Vec<_>>()]),
            other => json!([other.kind(), other.value()]),
        })
    });
    let _values = on_value(&*view, move |value| valued.push(value.clone()));
    editor.ready().await.unwrap();
    assert_eq!(editor.variant().as_deref(), Some("editor"));
    editor.call("edit", Some(json!({"text": "hi"})), None).await.unwrap();
    assert_eq!(shapes.all(), [json!("editor")]);
    // Listeners added while loading get their snapshot once the reference is live.
    let ops = json!(["ops", [["r", {"text": "hi"}]]]);
    assert_eq!(events.all(), [json!(["snap", {"text": ""}]), ops]);
    assert_eq!(values.all(), [json!({"text": ""}), json!({"text": "hi"})]);
}

#[tokio::test]
async fn only_host_lifetime_objects_register_and_tables_follow_registration_order() {
    let _serial = serial().await;
    let host = quiet();
    let routes = define_registry::<String>("t.routes", None);
    let entries = [("t.routes-a", vec!["GET /a", "GET /c"]), ("t.routes-b", vec!["GET /b"])];
    for (name, keys) in entries {
        let routes = routes.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let entries = keys.iter().map(|key| (key.to_string(), key.trim_start_matches("GET /").to_string()));
            decl.register(&routes, entries)?;
            Ok(Implementation::new())
        };
        host.serve(&empty(name), factory, ServeOptions::host()).unwrap();
    }
    let tables: Log<Vec<String>> = Log::default();
    let pushed = tables.clone();
    let _tables = host
        .registry(&routes)
        .subscribe(Box::new(move |table, _| pushed.push(table.keys().cloned().collect::<Vec<_>>())));
    tick().await;
    tick().await;
    let order = ["GET /a", "GET /c", "GET /b"];
    assert_eq!(host.registry(&routes).table(None).unwrap().keys().collect::<Vec<_>>(), order);
    assert_eq!(host.registry(&routes).get("GET /b", None).unwrap().as_deref(), Some("b"));
    assert_eq!(tables.all().last().unwrap(), &order);

    let lazy = empty("t.lazy-register");
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        decl.register(&routes, [("x".to_string(), "x".to_string())])?;
        Ok(Implementation::new())
    };
    host.serve(&lazy, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &lazy, json!({})).ready().await.unwrap_err();
    assert!(error.to_string().contains("only objects of lifetime \"host\" may register"), "{error}");
}

#[tokio::test]
async fn rejects_conflicting_registrations() {
    let _serial = serial().await;
    let errors = Log::default();
    let host = quiet_with(&errors);
    let routes = define_registry::<String>("t.routes", None);
    for name in ["t.conflict-a", "t.conflict-b"] {
        let routes = routes.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            decl.register(&routes, [("same".to_string(), name.to_string())])?;
            Ok(Implementation::new())
        };
        host.serve(&empty(name), factory, ServeOptions::host()).unwrap();
    }
    tick().await;
    tick().await;
    assert!(errors.all().iter().any(|error| error.to_string().contains("registry_conflict")), "{:?}", errors.all());
}

fn source() -> Contract {
    local_type("t.source", object(&[]), vec![("n", number().into()), ("set", method(number(), None).into())])
}

fn serve_source(host: &Host, first: Json) {
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let n = decl.state("n", number(), first.clone())?;
        Ok(Implementation::new()
            .state("n", n.clone())
            .method("set", move |value, ctx| ready(n.replace(&ctx, value).map(|()| Json::Null))))
    };
    host.serve(&source(), factory, ServeOptions::default()).unwrap();
}

#[tokio::test]
async fn reference_state_members_are_sources_for_listeners_and_derivations() {
    let _serial = serial().await;
    let host = quiet();
    serve_source(&host, json!(1));
    let mirror = local_type("t.mirror", object(&[]), vec![("copy", number().into()), ("doubled", number().into())]);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let source = decl.get(&source(), json!({}), GetOptions::default())?;
        let copy = decl.state("copy", number(), json!(0))?;
        let n: ReadableRef = source.state("n").unwrap();
        let target = copy.clone();
        decl.on("follow", n.clone(), move |value, ctx| {
            let value = if value.is_null() { json!(0) } else { value };
            ready(target.replace(&ctx, value))
        })?;
        let doubled = decl.derive(vec![n], |values| Ok(json!(values[0].as_f64().unwrap_or_default() * 2.0)));
        Ok(Implementation::new().state("copy", copy).state("doubled", doubled))
    };
    host.serve(&mirror, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &mirror, json!({}));
    reference.ready().await.unwrap();
    tick().await;
    let read = |name: &str| reference.state(name).unwrap().value().unwrap();
    assert_eq!(read("copy"), json!(1));
    assert_eq!(read("doubled"), json!(2));
    hold(&host, &source(), json!({})).call("set", Some(json!(5)), None).await.unwrap();
    tick().await;
    assert_eq!(read("copy"), json!(5));
    assert_eq!(read("doubled"), json!(10));
}

#[tokio::test]
async fn ctx_get_is_for_the_root_identity_at_ingress_and_for_the_host_inside_objects() {
    let _serial = serial().await;
    let host = keyed_host(|principal| principal.to_string());
    let seen: Log<Json> = Log::default();
    let who = local_type("t.who", object(&[]), vec![("ping", method(None, None).into())]);
    let front = local_type("t.front", object(&[]), vec![("relay", method(None, None).into())]);
    let pinged = seen.clone();
    let who_factory = move |_: &Json, principal: &Json, _: &mut Declare| {
        pinged.push(principal.clone());
        Ok(Implementation::new().method("ping", |_, _| ready(Ok(Json::Null))))
    };
    host.serve(&who, who_factory, ServeOptions::default()).unwrap();
    let (relayed, target) = (seen.clone(), who.clone());
    let front_factory = move |_: &Json, principal: &Json, _: &mut Declare| {
        relayed.push(principal.clone());
        let target = target.clone();
        Ok(Implementation::new().method("relay", move |_, ctx: Ctx| {
            let target = target.clone();
            async move {
                let who = ctx.get(&target, json!({}), GetOptions::default()).await?;
                who.call("ping", None, Some(&ctx)).await.map(|_| Json::Null)
            }
        }))
    };
    host.serve(&front, front_factory, ServeOptions::default()).unwrap();
    let run = move |ctx: Ctx| async move {
        let front = ctx.get(&front, json!({}), GetOptions::default()).await?;
        front.call("relay", None, Some(&ctx)).await
    };
    let options = RootOptions { principal: Some(json!({"user": "guest"})), ..RootOptions::default() };
    host.root(run, options).await.unwrap();
    assert_eq!(seen.all(), [json!({"user": "guest"}), json!({"kind": "host"})]);
}

#[tokio::test]
async fn calling_a_reference_inside_a_factory_fails_not_ready() {
    let _serial = serial().await;
    let host = quiet();
    serve_source(&host, json!(0));
    let eager = empty("t.eager-call");
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let source = decl.get(&source(), json!({}), GetOptions::default())?;
        drop(source.call("set", Some(json!(1)), None));
        Ok(Implementation::new())
    };
    host.serve(&eager, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &eager, json!({})).ready().await.unwrap_err();
    assert!(is(&error, "not_ready"), "{error}");
}

#[tokio::test]
async fn a_chord_error_from_a_factory_keeps_its_code_through_dependents() {
    let _serial = serial().await;
    let host = quiet();
    let (secret, page) = (empty("t.secret"), empty("t.page"));
    let refuse: Factory = |_, _, _| Err(forbidden(Some("not for you")));
    host.serve(&secret, refuse, ServeOptions::default()).unwrap();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        decl.get(&secret, json!({}), GetOptions::default())?;
        Ok(Implementation::new())
    };
    host.serve(&page, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &page, json!({})).ready().await.unwrap_err();
    assert!(is(&error, "forbidden"), "{error}");
}
