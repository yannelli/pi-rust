//! Port of upstream `test/reload.test.ts`, part 3: contract checks at reload, listeners and tasks of the new
//! code, registry tables per code version, and new objects of a changed type during the gate.

mod chord2_support;

use std::collections::HashMap;
use std::sync::Arc;

use chord2_support::reload::*;
use chord2_support::*;
use pi_chord::host2::object::{Adapter, Member};
use pi_chord::host2::{
    Contract, Declare, GetOptions, Implementation, RegistryView, RemoteDef, ServeOptions, StateOptions, VariantDef,
    define_facet, define_registry, host_named, method, remote,
};
use pi_chord::{Json, json};

fn doc(version: i64, method_name: &str, field: &str, schema: Json) -> Contract {
    let result = object(&[req(field, schema)]);
    let main = VariantDef::new().method(method_name, method(None, result));
    let def = RemoteDef {
        version,
        input: object(&[]),
        host: host_named("host").unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote("t.rdoc", &def).unwrap()
}

fn fetch() -> Member {
    Member::method(|_, _| std::future::ready(Ok(json!({"body": "v2"}))))
}

#[tokio::test]
async fn checks_contracts_at_reload_forgotten_bumps_live_consumers_and_accepts_adapters() {
    let _serial = serial().await;
    let doc_v1 = doc(1, "read", "text", string());
    let doc_broken = doc(1, "read", "text", number());
    let doc_v2 = doc(2, "fetch", "body", string());
    let host = quiet();
    let v1 = doc_v1.clone();
    let read = |text: Json| {
        Implementation::variant("main")
            .member("read", Member::method(move |_, _| std::future::ready(Ok(json!({"text": text.clone()})))))
    };
    install(
        &host,
        define_facet("doc", move |api| {
            api.serve(&v1, move |_: &Json, _: &Json, _: &mut Declare| Ok(read(json!("v1"))), Default::default())
        }),
    )
    .await;
    let consumer = hold(&host, &doc_v1, json!({}));
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!({"text": "v1"}));

    let broken = define_facet("doc", move |api| {
        api.serve(&doc_broken, move |_: &Json, _: &Json, _: &mut Declare| Ok(read(json!(1))), Default::default())
    });
    let error = reload(&host, broken).await.unwrap_err();
    assert!(error.to_string().contains("without a version bump"), "{error}");

    let v2 = doc_v2.clone();
    let plain = define_facet("doc", move |api| {
        let factory =
            |_: &Json, _: &Json, _: &mut Declare| Ok(Implementation::variant("main").member("fetch", fetch()));
        api.serve(&v2, factory, Default::default())
    });
    let error = reload(&host, plain).await.unwrap_err();
    assert!(is(&error, "contract_mismatch"), "{error}");

    let v2 = doc_v2.clone();
    let adapted = define_facet("doc", move |api| {
        let factory =
            |_: &Json, _: &Json, _: &mut Declare| Ok(Implementation::variant("main").member("fetch", fetch()));
        let adapter: Adapter = Arc::new(|current: &Implementation| {
            let Some(Member::Method(fetched)) = current.get("fetch").cloned() else { panic!("no fetch") };
            Implementation::variant("main").member(
                "read",
                Member::method(move |_, ctx| {
                    let fetched = fetched.clone();
                    async move {
                        let body = fetched(Json::Null, ctx).await?;
                        Ok(json!({"text": format!("{} via adapter", text(&body["body"]))}))
                    }
                }),
            )
        });
        let options = ServeOptions { accepts: Some(HashMap::from([(1, adapter)])), ..ServeOptions::default() };
        api.serve(&v2, factory, options)
    });
    reload(&host, adapted).await.unwrap();
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!({"text": "v2 via adapter"}));
    let modern = hold(&host, &doc_v2, json!({}));
    assert_eq!(modern.call_json("fetch", Json::Null, None).await.unwrap(), json!({"body": "v2"}));
}

#[tokio::test]
async fn rebinds_listeners_to_the_new_code_and_restarts_tasks() {
    let _serial = serial().await;
    let host = quiet();
    let events = Emitter::default();
    let log: Log = Log::default();
    let sum_schema = || object(&[req("v", number())]);
    let sum_type = local_type("t.sum", object(&[]), vec![("sum", sum_schema().into())]);
    let facet = |factor: f64| {
        let (events, log, sum_type) = (events.clone(), log.clone(), sum_type.clone());
        define_facet("sum", move |api| {
            let (events, log) = (events.clone(), log.clone());
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let sum = decl.state("sum", sum_schema(), json!({"v": 0}))?;
                let target = sum.clone();
                decl.on("add", events.source(), move |value, ctx| {
                    let sum = target.clone();
                    async move {
                        sum.change(&ctx, |draft| {
                            let v = draft["v"].as_f64().unwrap_or_default();
                            draft.set("v", v + value.as_f64().unwrap_or_default() * factor)
                        })
                    }
                })?;
                let log = log.clone();
                decl.task(move |ctx| {
                    let log = log.clone();
                    async move {
                        log.push(format!("start {factor}"));
                        ctx.signal().cancelled().await;
                        log.push(format!("stop {factor}"));
                        Ok(())
                    }
                });
                Ok(Implementation::new().state("sum", sum))
            };
            api.serve(&sum_type, factory, Default::default())
        })
    };
    install(&host, facet(1.0)).await;
    let reference = hold(&host, &sum_type, json!({}));
    reference.ready().await.unwrap();
    events.emit(json!(1));
    tick().await;
    reload(&host, facet(10.0)).await.unwrap();
    assert_eq!(events.size(), 1);
    events.emit(json!(1));
    tick().await;
    assert_eq!(state_of(&reference, "sum"), json!({"v": 11}));
    // The replacement starts once the aborted run finished.
    until(|| log.len() >= 3).await;
    assert_eq!(log.all(), ["start 1", "stop 1", "start 10"]);
}

#[tokio::test]
async fn keeps_registry_tables_per_code_version() {
    let _serial = serial().await;
    let host = quiet();
    let routes = define_registry::<String>("t.reload-routes", None);
    let router = local_type("t.router", object(&[]), vec![]);
    let facet = |value: &'static str| {
        let (routes, router) = (routes.clone(), router.clone());
        define_facet("router", move |api| {
            let routes = routes.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                decl.register(&routes, [("GET /".to_string(), value.to_string())])?;
                Ok(Implementation::new())
            };
            api.serve(&router, factory, ServeOptions::host())
        })
    };
    install(&host, facet("one")).await;
    tick().await;
    let seen: Log<(Option<String>, i64)> = Log::default();
    let record = seen.clone();
    let _subscription = host.registry(&routes).subscribe(Box::new(move |table, code_version| {
        record.push((table.get("GET /").cloned(), code_version));
    }));
    let go = Latch::default();
    let pinned = spawn_root(&host, {
        let (go, view) = (go.clone(), host.registry(&routes));
        move |ctx| async move {
            go.wait().await;
            Ok(json!(view.get("GET /", Some(ctx.code_version()))?))
        }
    });
    tick().await;
    let report = reload(&host, facet("two")).await.unwrap();
    assert_eq!(host.registry(&routes).get("GET /", None).unwrap().as_deref(), Some("two"));
    go.open(Json::Null);
    assert_eq!(pinned.await.unwrap().unwrap(), json!("one"));
    let expected = [(Some("one".to_string()), report.previous), (Some("two".to_string()), report.code_version)];
    assert_eq!(seen.all(), expected);
}

#[tokio::test]
async fn the_gate_refuses_new_objects_of_a_changed_type_and_the_commit_gives_them_new_code() {
    let _serial = serial().await;
    let host = quiet();
    install(&host, greeter_facet(Spec::new("hello"))).await;
    let reference = hold_greeter(&host, json!({}));
    reference.ready().await.unwrap();
    let go = Latch::default();
    let grandfathered = spawn_root(&host, {
        let (reference, go) = (reference.clone(), go.clone());
        move |ctx| async move {
            greet_in(&reference, "g", &ctx).await?;
            go.wait().await;
            Ok(Json::Null)
        }
    });
    let later = Latch::default();
    let pinned = spawn_root(&host, {
        let (reference, later) = (reference.clone(), later.clone());
        move |ctx| async move {
            later.wait().await;
            Ok(json!(greet_in(&reference, "old", &ctx).await?))
        }
    });
    tick().await;
    let v2 = Spec::new("hi").state(StateOptions { version: Some(2), migrate: None });
    let reloading = tokio::spawn(reload(&host, greeter_facet(v2)));
    until_conflict(&reference).await;
    let error = host.hold(&greeter(), json!({"id": "late"}), GetOptions::default()).unwrap_err();
    assert!(is(&error, "upgrade_conflict") && error.retryable(), "{error}");
    go.open(Json::Null);
    grandfathered.await.unwrap().unwrap();
    reloading.await.unwrap().unwrap();
    assert_eq!(greet(&hold_greeter(&host, json!({"id": "late"})), "l").await.unwrap(), "hi l");
    // The older code version is still kept (pinned root), but the gated object dropped its old implementation.
    assert!(host.previous_code_version().is_some());
    later.open(Json::Null);
    let error = pinned.await.unwrap().unwrap_err();
    assert!(is(&error, "upgrade_conflict"), "{error}");
}
