//! Port of upstream `test/cluster.test.ts`, part 1: cluster reloads of a server and its workers, failing votes,
//! removals and their tombstones, and browsers. The reconciliation cases are in `chord2_cluster_reconcile.rs`.

mod chord2_support;

use std::sync::Arc;

use chord2_support::cluster::*;
use chord2_support::net::host_options;
use chord2_support::*;
use pi_chord::host2::declare::ResourceDeclaration;
use pi_chord::host2::wire::PeerKind;
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Declare, GetOptions, HostOptions, Implementation, Link, RefStatus, ReloadOptions,
    Resolve, Route, create_host, define_facet, memory_link_pair,
};
use pi_chord::{Json, json};

fn hold_session(host: &pi_chord::host2::Host, session: &str) -> pi_chord::host2::Ref {
    host.hold(&greeter(), json!({"session": session}), GetOptions::default()).unwrap()
}

async fn greet(reference: &pi_chord::host2::Ref, name: &str) -> String {
    text(&reference.call_json("greet", json!(name), None).await.unwrap()).to_string()
}

#[tokio::test]
async fn prepares_every_host_commits_the_server_first_then_the_workers() {
    let _serial = serial().await;
    let Setup { server, worker, modules, .. } = setup().await;
    tick().await;
    let greeter = hold_session(&server, "S1");
    assert_eq!(greet(&greeter, "a").await, "v1 a");
    let go = Latch::default();
    let pinned = {
        let (greeter, go) = (greeter.clone(), go.clone());
        tokio::spawn(server.root(
            move |ctx| async move {
                greeter.call_json("greet", json!("p"), Some(&ctx)).await?;
                go.wait().await;
                greeter.call_json("greet", json!("p"), Some(&ctx)).await
            },
            Default::default(),
        ))
    };
    tick().await;
    let shapes: Log = Log::default();
    let seen = shapes.clone();
    let _shape = greeter.on_shape(Arc::new(move |shape| seen.push(shape.contract.clone())));
    modules.set("greeter", greeter_facet("v2"));
    let report = server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    // No waiting: the commit frame precedes this call on the FIFO link, so the worker committed when it arrives.
    assert_eq!(greet(&greeter, "b").await, "v2 b");
    assert_eq!(server.code_version(), report.code_version);
    assert_eq!(worker.code_version(), report.code_version);
    go.open(Json::Null);
    assert_eq!(pinned.await.unwrap().unwrap(), json!("v1 p"));
    assert_eq!(shapes.len(), 1, "one shape push at the commit");
    // The server retires the old code version once its last old root ended, then tells the worker.
    until(|| worker.previous_code_version().is_none()).await;
    assert_eq!(server.previous_code_version(), None);
}

#[tokio::test]
async fn a_failing_vote_aborts_every_host_and_the_error_keeps_its_code_and_names_the_host() {
    let _serial = serial().await;
    let Setup { server, worker, modules, .. } = setup().await;
    tick().await;
    let greeter = hold_session(&server, "S1");
    greet(&greeter, "a").await;
    let before = [server.code_version(), worker.code_version()];
    modules.set("greeter", greeter_facet_with("v2", true, 1));
    let error = server.reload(vec![], remote_options(&["greeter"])).await.unwrap_err();
    assert!(is(&error, "unavailable"), "{error}");
    let message = error.to_string();
    assert!(message.contains("session:S1: ") && message.contains("worker factory fails"), "{message}");
    assert_eq!([server.code_version(), worker.code_version()], before);
    assert_eq!(greet(&greeter, "x").await, "v1 x");
}

#[tokio::test]
async fn a_worker_gate_blocked_past_its_deadline_aborts_the_whole_reload() {
    let _serial = serial().await;
    let Setup { server, worker, modules, .. } = setup().await;
    tick().await;
    let greeter = hold_session(&server, "S1");
    let go = Latch::default();
    let blocker = {
        let (greeter, go) = (greeter.clone(), go.clone());
        tokio::spawn(server.root(
            move |ctx| async move {
                greeter.call_json("greet", json!("g"), Some(&ctx)).await?;
                go.wait().await;
                Ok(())
            },
            Default::default(),
        ))
    };
    tick().await;
    let before = [server.code_version(), worker.code_version()];
    modules.set("greeter", greeter_facet_with("v2", false, 2));
    let options = ReloadOptions { gate_deadline: Some(reload::ms(30)), ..remote_options(&["greeter"]) };
    let error = server.reload(vec![], options).await.unwrap_err();
    assert!(is(&error, "aborted") && error.reason() == Some("gate_deadline"), "{error}");
    let message = error.to_string();
    let blocking = message.split("session:S1: ").nth(1).and_then(|rest| rest.split("blocking roots: ").nth(1));
    assert!(blocking.is_some_and(|roots| roots.starts_with("server#")), "{message}");
    assert_eq!([server.code_version(), worker.code_version()], before);
    go.open(Json::Null);
    blocker.await.unwrap().unwrap();
    assert_eq!(greet(&greeter, "x").await, "v1 x");
}

#[tokio::test]
async fn removes_facets_reconciles_removal_tombstones_and_clears_them_on_re_add() {
    let _serial = serial().await;
    let Setup { server, worker, modules, .. } = setup().await;
    server.install(&greeter_facet("v1"), Default::default()).await.unwrap();
    tick().await;
    let local = server.hold(&server_greeter(), json!({}), GetOptions::default()).unwrap();
    let child = hold_session(&server, "S1");
    assert_eq!(greet(&local, "a").await, "v1 a");
    assert_eq!(greet(&child, "a").await, "v1 a");
    let remove = ReloadOptions {
        remove: vec!["greeter".into()],
        remote_remove: Some([("session:".to_string(), vec!["greeter".to_string()])].into()),
        ..ReloadOptions::default()
    };
    server.reload(vec![], remove).await.unwrap();
    until(|| child.status() == RefStatus::Gone).await;
    assert_eq!(local.status(), RefStatus::Gone);

    // A child starting with the old set must remove it without loading its removed module.
    let cold = unconnected("cold", Some(Arc::new(NoModules)));
    cold.install(&greeter_facet("v1"), Default::default()).await.unwrap();
    let old = hold_session(&cold, "cold");
    old.ready().await.unwrap();
    join(&server, &cold, "cold");
    until(|| old.status() == RefStatus::Gone).await;
    assert_eq!(cold.code_version(), server.code_version());
    // Close this loader-less child before the addition; it tested only the removal reconciliation.
    cold.close().await.unwrap();
    modules.set("greeter", greeter_facet("v2"));
    server.reload(vec![greeter_facet("v2").into()], remote_options(&["greeter"])).await.unwrap();
    assert_eq!(greet(&hold_session(&server, "S1"), "b").await, "v2 b");
    let fresh = unconnected("fresh", modules.loader());
    fresh.install(&greeter_facet("v1"), Default::default()).await.unwrap();
    join(&server, &fresh, "fresh");
    assert_eq!(greet(&hold_session(&server, "fresh"), "c").await, "v2 c");
    for host in [fresh, worker, server] {
        host.close().await.unwrap();
    }
}

#[tokio::test]
async fn only_the_server_coordinates_reloads() {
    let _serial = serial().await;
    let Setup { worker, .. } = setup().await;
    tick().await;
    let error = worker.reload(vec![], ReloadOptions::default()).await.unwrap_err();
    assert!(is(&error, "unavailable") && error.reason() == Some("not_coordinator"), "{error}");
}

#[tokio::test]
async fn browsers_are_not_participants_they_see_shape_pushes_or_upgrade_required() {
    let _serial = serial().await;
    let Setup { server, modules, down: worker_link, .. } = setup().await;
    let (down, up) = memory_link_pair();
    // The route resolves to the worker connected already.
    let resolve: Resolve = Arc::new(move |_: &str| {
        let link: Arc<dyn Link> = worker_link.clone();
        Box::pin(std::future::ready(Ok(link)))
    });
    let authorize: pi_chord::host2::Authorize = Arc::new(|_: &Json, _: &str| Box::pin(std::future::ready(true)));
    server.route("session:", Route { authorize, resolve, principal: Arc::new(|name: &str| json!({"host": name})) });
    server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up, ConnectOptions::new(json!({"kind": "server"}))));
    let browser =
        create_host(HostOptions { kind: PeerKind::Client, upstream, ..host_options("browser", &Log::default()) });
    let viewer = hold_session(&browser, "S1");
    assert_eq!(greet(&viewer, "a").await, "v1 a");
    let shapes: Log = Log::default();
    let seen = shapes.clone();
    let _shape = viewer.on_shape(Arc::new(move |shape| seen.push(shape.contract.clone())));
    modules.set("greeter", greeter_facet("v2"));
    server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    until(|| !shapes.is_empty()).await;
    assert_eq!(shapes.len(), 1, "one shape push at the commit");
    assert_eq!(greet(&viewer, "b").await, "v2 b");

    let hello = define_facet("greeter", |api| {
        let factory = |_: &Json, _: &Json, _: &mut Declare| {
            Ok(Implementation::variant("main")
                .method("hello", |name, _| std::future::ready(Ok(json!(format!("hi {}", text(&name)))))))
        };
        api.serve(&greeter_v2(), factory, Default::default())
    });
    modules.set("greeter", hello);
    server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    until(|| viewer.status() == RefStatus::Gone).await;
    let error = viewer.error().expect("gone with an error");
    assert!(is(&error, "upgrade_required"), "{error}");
}

#[tokio::test]
async fn an_abort_while_a_worker_prepares_closes_a_late_open_and_the_worker_reloads_again() {
    let _serial = serial().await;
    let Setup { server, worker, modules, .. } = setup().await;
    tick().await;
    let greeter = hold_session(&server, "S1");
    greet(&greeter, "a").await;
    let log: Log = Log::default();
    let go = Latch::default();
    let (opens, closes, gate) = (log.clone(), log.clone(), go.clone());
    let slow = define_facet("greeter", move |api| {
        let (opens, closes, gate) = (opens.clone(), closes.clone(), gate.clone());
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let (opens, closes, gate) = (opens.clone(), closes.clone(), gate.clone());
            let open = move |_| {
                let (opens, gate) = (opens.clone(), gate.clone());
                async move {
                    gate.wait().await;
                    opens.push("opened");
                    Ok(())
                }
            };
            let late = ResourceDeclaration::new(1, open).with_close(move |_: ()| closes.push_ready("closed"));
            decl.resource("late", late)?;
            let count = decl.state("count", object(&[req("n", number())]), json!({"n": 0}))?;
            Ok(Implementation::variant("main")
                .state("count", count)
                .method("greet", |name, _| std::future::ready(Ok(json!(format!("slow {}", text(&name)))))))
        };
        api.serve(&chord2_support::cluster::greeter(), factory, Default::default())
    });
    modules.set("greeter", slow);
    let code_version = worker.code_version();
    let options = ReloadOptions { vote_deadline: Some(reload::ms(50)), ..remote_options(&["greeter"]) };
    let error = server.reload(vec![], options).await.unwrap_err();
    assert!(is(&error, "aborted"), "{error}");
    go.open(Json::Null);
    until(|| log.all().contains(&"closed".to_string())).await;
    assert_eq!(log.all(), ["opened", "closed"]);
    assert_eq!(worker.code_version(), code_version);
    modules.set("greeter", greeter_facet("v3"));
    server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    assert_eq!(greet(&greeter, "b").await, "v3 b");
}
