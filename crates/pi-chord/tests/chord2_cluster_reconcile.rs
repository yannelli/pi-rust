//! Port of upstream `test/cluster.test.ts`, part 2: children that connect behind or during a reload, the retire
//! watermark, and forced retirement of a call relayed to a worker.

mod chord2_support;

use std::sync::Arc;
use std::time::Instant;

use chord2_support::cluster::*;
use chord2_support::net::host_options;
use chord2_support::*;
use pi_chord::host2::declare::ResourceDeclaration;
use pi_chord::host2::wire::{Frame, PeerKind};
use pi_chord::host2::{
    AcceptOptions, Authorize, ConnectOptions, Contract, Declare, Facet, GetOptions, Host, HostOptions, Implementation,
    Link, ReloadOptions, RemoteDef, Resolve, Route, ServeOptions, VariantDef, create_host, define_facet, host_named,
    memory_link_pair, method, remote,
};
use pi_chord::{Json, json};

fn hold_session(host: &Host, session: &str) -> pi_chord::host2::Ref {
    host.hold(&greeter(), json!({"session": session}), GetOptions::default()).unwrap()
}

async fn greet(reference: &pi_chord::host2::Ref, name: &str) -> String {
    text(&reference.call_json("greet", json!(name), None).await.unwrap()).to_string()
}

#[tokio::test]
async fn a_child_saying_hello_during_a_reload_is_reconciled_after_it() {
    let _serial = serial().await;
    let modules = Modules::with("greeter", greeter_facet("v1"));
    let loading = Modules::stubs();
    loading.block(true);
    let server = server_loading(&loading);
    let reloading = server.reload(vec!["server-plugin".into()], remote_options(&["greeter"]));
    // The worker connects while the reload loads its modules: it announces the server's current code version.
    let (worker, _, _) = worker(&server, "S1", Some(&modules));
    worker.install(&modules.get("greeter"), Default::default()).await.unwrap();
    tick().await;
    modules.set("greeter", greeter_facet("v2"));
    loading.open();
    let report = reloading.await.unwrap();
    assert!(report.children.is_empty(), "it was not a participant");
    assert_eq!(greet(&hold_session(&server, "S1"), "a").await, "v2 a");
    assert_eq!(worker.code_version(), server.code_version());
    worker.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_child_keeps_the_previous_code_version_until_its_parent_retired_it() {
    let _serial = serial().await;
    let Setup { server, worker, modules, .. } = setup().await;
    tick().await;
    let greeter = hold_session(&server, "S1");
    assert_eq!(greet(&greeter, "a").await, "v1 a");
    let first = server.code_version();
    let go = Latch::default();
    // A server root of the first code version that calls the worker only after the next two reloads started.
    let pinned = {
        let (greeter, go) = (greeter.clone(), go.clone());
        tokio::spawn(server.root(
            move |ctx| async move {
                go.wait().await;
                greeter.call_json("greet", json!("p"), Some(&ctx)).await
            },
            Default::default(),
        ))
    };
    modules.set("greeter", greeter_facet("v2"));
    server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    assert_eq!(worker.previous_code_version(), Some(first));
    let options = ReloadOptions { retire_deadline: Some(reload::ms(2000)), ..remote_options(&[]) };
    let second = server.reload(vec![], options);
    tick().await;
    tick().await;
    // The server still waits for its root, so the worker must not have retired that code version.
    assert_eq!(worker.previous_code_version(), Some(first));
    go.open(Json::Null);
    assert_eq!(pinned.await.unwrap().unwrap(), json!("v1 p"));
    second.await.unwrap();
    assert_eq!(server.previous_code_version(), None);
}

#[tokio::test]
async fn a_child_reconciled_after_its_parent_retired_old_versions_retires_its_previous_at_once() {
    let _serial = serial().await;
    let modules = Modules::with("greeter", greeter_facet("v1"));
    let server = server();
    server.install(&empty_facet("server-plugin"), Default::default()).await.unwrap();
    for _ in 0..2 {
        server.reload(vec![empty_facet("server-plugin").into()], ReloadOptions::default()).await.unwrap();
    }
    assert_eq!(server.previous_code_version(), None);
    let (worker, _, _) = worker(&server, "S1", Some(&modules));
    worker.install(&modules.get("greeter"), Default::default()).await.unwrap();
    until(|| worker.code_version() == server.code_version()).await;
    // Without the parent's retire watermark it would keep code version 1 until the parent's next retirement.
    until(|| worker.previous_code_version().is_none()).await;
    // And the next cluster reload does not wait for a retirement the parent will never announce.
    let started = Instant::now();
    let options = ReloadOptions { retire_deadline: Some(reload::ms(10_000)), ..remote_options(&["greeter"]) };
    server.reload(vec![], options).await.unwrap();
    assert!(started.elapsed().as_millis() < 2000);
    worker.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_reconciliation_running_when_a_reload_starts_syncs_the_child_after_it() {
    let _serial = serial().await;
    let modules = Modules::with("greeter", greeter_facet("v1"));
    let server_modules = Modules::stubs();
    let server = server_loading(&server_modules);
    server.install(&empty_facet("server-plugin"), Default::default()).await.unwrap();
    server.reload(vec!["server-plugin".into()], remote_options(&["greeter"])).await.unwrap();
    // The worker connects behind: its reconciliation to code version 2 waits in the worker's module loading.
    modules.block(true);
    let (worker, _, _) = worker(&server, "S1", Some(&modules));
    worker.install(&modules.get("greeter"), Default::default()).await.unwrap();
    tick().await;
    // A reload starts before the reconciliation committed; the worker is not one of its participants.
    server_modules.block(true);
    modules.set("greeter", greeter_facet("v2"));
    let reloading = server.reload(vec!["server-plugin".into()], remote_options(&["greeter"]));
    modules.block(false);
    modules.open();
    until(|| worker.code_version() == 2).await;
    server_modules.open();
    let report = reloading.await.unwrap();
    assert!(report.children.is_empty());
    assert_eq!(report.code_version, 3);
    assert_eq!(greet(&hold_session(&server, "S1"), "a").await, "v2 a");
    assert_eq!(worker.code_version(), server.code_version());
    worker.close().await.unwrap();
    server.close().await.unwrap();
}

fn ping() -> Contract {
    let main = VariantDef::new().method("ping", method(None, string()));
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named("server").unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote("t.cluster-ping", &def).unwrap()
}

fn ping_facet() -> Facet {
    define_facet("ping", |api| {
        let factory = |_: &Json, _: &Json, _: &mut Declare| {
            Ok(Implementation::variant("main").method("ping", |_, _| std::future::ready(Ok(json!("pong")))))
        };
        api.serve(&ping(), factory, Default::default())
    })
}

#[tokio::test]
async fn a_child_behind_whose_hello_waits_for_a_reload_is_fenced_at_once_and_its_calls_up_are_admitted() {
    let _serial = serial().await;
    let loading = Modules::stubs();
    let server = server_loading(&loading);
    server.install(&ping_facet(), Default::default()).await.unwrap();
    server.reload(vec![ping_facet().into()], ReloadOptions::default()).await.unwrap();
    assert_eq!(server.previous_code_version(), None, "code version 1 is retired");
    loading.block(true);
    let reloading = server.reload(vec!["server-plugin".into()], remote_options(&[]));
    let (worker, _, _) = worker(&server, "S1", None);
    // While the server's reload still loads its modules, a worker root (code version 1) calls the server.
    let run = |ctx: pi_chord::host2::Ctx| async move {
        let ping = ctx.get(&ping(), json!({}), GetOptions::default()).await?;
        ping.call_json("ping", Json::Null, Some(&ctx)).await
    };
    assert_eq!(worker.root(run, Default::default()).await.unwrap(), json!("pong"));
    loading.open();
    reloading.await.unwrap();
    until(|| worker.code_version() == server.code_version()).await;
    worker.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_reconnecting_child_still_keeping_its_previous_version_reconciles_without_the_retire_deadline() {
    let _serial = serial().await;
    let modules = Modules::with("greeter", greeter_facet("v1"));
    let server = server();
    let (down, up) = memory_link_pair();
    // The `retire` frame is lost: the worker keeps code version 1, not allowed to retire it.
    let dropping = Dropping::wrap(down, |frame| matches!(frame, Frame::Retire { .. }));
    server.accept(dropping, AcceptOptions::host("session:S1", json!({"worker": "w"}))).unwrap();
    let worker = unconnected("S1", modules.loader());
    worker.connect(up.clone(), ConnectOptions::new(json!({"kind": "server"})));
    worker.install(&modules.get("greeter"), Default::default()).await.unwrap();
    tick().await;
    modules.set("greeter", greeter_facet("v2"));
    server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    tick().await;
    assert_eq!(worker.previous_code_version(), Some(1));
    up.close(Some("gone"));
    tick().await;
    // The server moves on while the worker is away, then the worker reconnects behind it.
    modules.set("greeter", greeter_facet("v3"));
    server.reload(vec![empty_facet("server-plugin").into()], ReloadOptions::default()).await.unwrap();
    let started = Instant::now();
    join(&server, &worker, "S1");
    for _ in 0..400 {
        if worker.code_version() == server.code_version() {
            break;
        }
        sleep_ms(5).await;
    }
    assert_eq!(worker.code_version(), server.code_version());
    assert!(started.elapsed().as_millis() < 2000, "reconciled after {:?}", started.elapsed());
    worker.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn forced_retirement_cancels_a_relayed_browser_call_before_closing_the_old_resources() {
    let _serial = serial().await;
    let log: Log = Log::default();
    let server = server();
    let (down, up) = memory_link_pair();
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up, ConnectOptions::new(json!({"kind": "server"}))));
    let worker = create_host(HostOptions { upstream, ..host_options("session:S1", &Log::default()) });
    let never = Latch::default();
    let main = VariantDef::new().method("wait", method(None, None));
    let host = host_named("session:S1").unwrap();
    let slow = remote(
        "t.cluster-slow",
        &RemoteDef { version: 1, input: object(&[]), host, variants: vec![("main".into(), main)] },
    )
    .unwrap();
    let (aborts, waits) = (log.clone(), never.clone());
    let factory = move |_: &Json, _: &Json, _: &mut Declare| {
        let (aborts, waits) = (aborts.clone(), waits.clone());
        Ok(Implementation::variant("main").method("wait", move |_, ctx| {
            let (aborts, waits) = (aborts.clone(), waits.clone());
            ctx.signal().add_listener(move |_| aborts.push("worker aborted"));
            async move {
                waits.wait().await;
                Ok(Json::Null)
            }
        }))
    };
    worker.serve(&slow, factory, ServeOptions::default()).unwrap();
    // Records the cancel the server sends down: it must leave before the old resources close.
    let cancels = log.clone();
    let watched = Dropping::wrap(down, move |frame| {
        if matches!(frame, Frame::Cancel { .. }) {
            cancels.push("cancel sent");
        }
        false
    });
    let resolve: Resolve = Arc::new(move |_: &str| Box::pin(std::future::ready(Ok(watched.clone()))));
    let authorize: Authorize = Arc::new(|_: &Json, _: &str| Box::pin(std::future::ready(true)));
    server.route("session:", Route { authorize, resolve, principal: Arc::new(|name: &str| json!({"host": name})) });
    let store = local_type("t.cluster-store", object(&[]), vec![]);
    let store_facet = |version: i64| {
        let (log, store) = (log.clone(), store.clone());
        define_facet("store", move |api| {
            let log = log.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let closes = log.clone();
                let conn = ResourceDeclaration::value(version, ())
                    .with_close(move |_: ()| closes.push_ready(format!("server closes {version}")));
                decl.resource("conn", conn)?;
                Ok(Implementation::new())
            };
            api.serve(&store, factory, ServeOptions::host())
        })
    };
    server.install(&store_facet(1), Default::default()).await.unwrap();
    let (browser_down, browser_up) = memory_link_pair();
    server.accept(browser_down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> =
        Some((browser_up, ConnectOptions::new(json!({"kind": "server"}))));
    let browser =
        create_host(HostOptions { kind: PeerKind::Client, upstream, ..host_options("browser", &Log::default()) });
    // The relayed call's root runs in the server's code version 1.
    let reference = browser.hold(&slow, json!({}), GetOptions::default()).unwrap();
    let call = tokio::spawn(async move { reference.call("wait", None, None).await });
    sleep_ms(20).await;
    server.reload(vec![store_facet(2).into()], ReloadOptions::default()).await.unwrap();
    let options = ReloadOptions { retire_deadline: Some(reload::ms(20)), ..ReloadOptions::default() };
    server.reload(vec![store_facet(3).into()], options).await.unwrap();
    let settled = tokio::time::timeout(reload::ms(2000), call).await.expect("settled").unwrap();
    assert!(settled.is_err(), "the browser's call is cancelled with its root");
    until(|| log.all().contains(&"worker aborted".to_string())).await;
    let order: Vec<String> =
        log.all().into_iter().filter(|entry| entry == "cancel sent" || entry == "server closes 1").collect();
    assert_eq!(order, ["cancel sent", "server closes 1"]);
    never.open(Json::Null);
    for host in [browser, worker, server] {
        host.close().await.unwrap();
    }
}
