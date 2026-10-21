//! Port of upstream `test/links-review.test.ts`, the reload cases: "#1 root_end is honored only from the link the
//! root came over" and the cross-host reloads (#3 to #6). The greeter here is the cluster tests' one, placed on
//! the `session:` family (upstream's is named `session:S1`).

mod chord2_support;

use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::cluster::*;
use chord2_support::net::{host_options, named};
use chord2_support::reload::ms;
use chord2_support::*;
use pi_chord::host2::wire::{Frame, Hello, PROTOCOL_VERSION, PeerKind};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Ctx, Declare, Facet, GetOptions, Host, HostOptions, Implementation,
    Initial, Link, Ref, ReloadOptions, RemoteDef, RootOptions, ServeOptions, StateOptions, VariantDef, create_host,
    define_facet, host_named, memory_link_pair, method, remote,
};
use pi_chord::{Json, json};

fn on(name: &str, host: &str, method_name: &str, spec: pi_chord::host2::MethodSpec) -> Contract {
    let main = VariantDef::new().method(method_name, spec);
    let host = host_named(host).unwrap();
    remote(name, &RemoteDef { version: 1, input: object(&[]), host, variants: vec![("main".into(), main)] }).unwrap()
}

/// Serves `contract` with a state cell `n` of `version` and the method `method_name` answering `answer`.
fn versioned_facet(contract: &Contract, version: i64, method_name: &'static str, answer: fn(&Ctx) -> Json) -> Facet {
    let contract = contract.clone();
    define_facet("echo", move |api| {
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let options = StateOptions { version: Some(version), migrate: None };
            decl.state_with("n", number(), Initial::Value(json!(0)), options)?;
            Ok(Implementation::variant("main").method(method_name, move |_, ctx| ready(Ok(answer(&ctx)))))
        };
        api.serve(&contract, factory, Default::default())
    })
}

fn root_id_of(ctx: &Ctx) -> Json {
    json!(ctx.root_id())
}

fn greeter_ref(host: &Host) -> Ref {
    host.hold(&greeter(), json!({"session": "S1"}), GetOptions::default()).unwrap()
}

async fn greet(reference: &Ref, name: &str) -> String {
    text(&reference.call_json("greet", json!(name), None).await.unwrap()).to_string()
}

#[tokio::test]
async fn r1_root_end_is_honored_only_from_the_link_the_root_came_over() {
    let _serial = serial().await;
    let server = named("server");
    let echo = on("t.echo", "server", "root", method(None, string()));
    server.install(&versioned_facet(&echo, 1, "root", root_id_of), Default::default()).await.unwrap();
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::host("w", json!({"worker": "w"}))).unwrap();
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up, ConnectOptions::new(json!({"kind": "server"}))));
    let worker = create_host(HostOptions { upstream, ..host_options("w", &Log::default()) });
    let (go, seen) = (Latch::default(), Arc::new(Mutex::new(String::new())));
    let pinned = tokio::spawn(worker.root(
        {
            let (go, seen, echo) = (go.clone(), seen.clone(), echo.clone());
            move |ctx| async move {
                let echo = ctx.get(&echo, json!({}), GetOptions::default()).await?;
                *seen.lock().unwrap() = text(&echo.call_json("root", Json::Null, Some(&ctx)).await?).to_string();
                go.wait().await;
                Ok(())
            }
        },
        RootOptions::default(),
    ));
    until(|| !seen.lock().unwrap().is_empty()).await;
    let root_id = seen.lock().unwrap().clone();
    // A client forging the worker root's end must not release its lease.
    let (client_down, raw) = memory_link_pair();
    server.accept(client_down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    raw.receive(Arc::new(|_| {}));
    let name = "raw".to_string();
    let hello = Hello { protocol: Some(PROTOCOL_VERSION), name, kind: PeerKind::Client, code_version: 0, acks: None };
    raw.send(&Frame::Hello(hello));
    raw.send(&Frame::RootEnd { root_id: root_id.clone() });
    sleep_ms(10).await;
    let options = ReloadOptions { gate_deadline: Some(ms(40)), ..ReloadOptions::default() };
    let error = server.reload(vec![versioned_facet(&echo, 2, "root", root_id_of).into()], options).await.unwrap_err();
    assert!(is(&error, "aborted") && error.reason() == Some("gate_deadline"), "{error}");
    assert!(error.to_string().contains(&root_id), "{error}");
    go.open(Json::Null);
    pinned.await.unwrap().unwrap();
}

#[tokio::test]
async fn r3_an_abort_while_the_child_is_still_loading_modules_frees_its_reload_slot() {
    let _serial = serial().await;
    let Setup { server, worker, modules, .. } = setup().await;
    tick().await;
    modules.set("greeter", greeter_facet("v2"));
    modules.block(true);
    let options = ReloadOptions { vote_deadline: Some(ms(30)), ..remote_options(&["greeter"]) };
    let error = server.reload(vec![], options).await.unwrap_err();
    assert!(is(&error, "aborted"), "{error}");
    modules.open();
    until(|| !worker.reloading()).await;
    server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    assert_eq!(greet(&greeter_ref(&server), "x").await, "v2 x");
}

#[tokio::test]
async fn r3_the_parent_link_dying_while_the_child_prepares_unwinds_the_child() {
    let _serial = serial().await;
    let Setup { server, worker, modules, down, .. } = setup().await;
    tick().await;
    modules.set("greeter", greeter_facet("v2"));
    modules.block(true);
    let reload = tokio::spawn(server.reload(vec![], remote_options(&["greeter"])));
    sleep_ms(5).await;
    down.close(Some("gone"));
    reload.await.unwrap().unwrap_err();
    modules.open();
    until(|| !worker.reloading()).await;
    assert_eq!(worker.code_version(), 1);
}

#[tokio::test]
async fn r4_tasks_the_commit_starts_cannot_reach_a_child_before_the_childs_commit() {
    let _serial = serial().await;
    let Setup { server, modules, .. } = setup().await;
    tick().await;
    let results: Log<Json> = Log::default();
    let caller = local_type("t.caller", object(&[]), vec![]);
    let caller_facet = |active: bool| {
        let (caller, results) = (caller.clone(), results.clone());
        define_facet("caller", move |api| {
            let results = results.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let target = decl.get(&greeter(), json!({"session": "S1"}), GetOptions::default())?;
                if active {
                    let results = results.clone();
                    decl.task(move |ctx| {
                        let (target, results) = (target.clone(), results.clone());
                        async move {
                            match target.call_json("greet", json!("t"), Some(&ctx)).await {
                                Ok(answer) => results.push(answer),
                                Err(error) => results.push(json!(code_of(&error))),
                            }
                            Ok(())
                        }
                    });
                }
                Ok(Implementation::new())
            };
            api.serve(&caller, factory, ServeOptions::host())
        })
    };
    server.install(&caller_facet(false), Default::default()).await.unwrap();
    sleep_ms(10).await;
    modules.set("greeter", greeter_facet("v2"));
    server.reload(vec![caller_facet(true).into()], remote_options(&["greeter"])).await.unwrap();
    until(|| results.len() == 1).await;
    assert_eq!(results.all(), [json!("v2 t")]);
}

#[tokio::test]
async fn r5_a_child_that_missed_a_commit_is_reconciled_on_reconnect_never_relabeled() {
    let _serial = serial().await;
    let modules = Modules::with("greeter", greeter_facet("v1"));
    let server = server();
    let (down, up) = memory_link_pair();
    // The commit frame is lost: the link dies right after the child voted.
    let closing = down.clone();
    let watched = Dropping::wrap(down, move |frame| {
        let commit = matches!(frame, Frame::Commit { .. });
        if commit {
            closing.close(Some("dropped"));
        }
        commit
    });
    server.accept(watched, AcceptOptions::host("session:S1", json!({"worker": "S1"}))).unwrap();
    let worker = unconnected("S1", modules.loader());
    worker.connect(up, ConnectOptions::new(json!({"kind": "server"})));
    worker.install(&modules.get("greeter"), Default::default()).await.unwrap();
    tick().await;
    modules.set("greeter", greeter_facet("v2"));
    server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    until(|| !worker.reloading()).await;
    assert_ne!(worker.code_version(), server.code_version());
    join(&server, &worker, "S1");
    assert_eq!(greet(&greeter_ref(&server), "x").await, "v2 x");
    assert_eq!(worker.code_version(), server.code_version());
}

#[tokio::test]
async fn r6_a_server_root_that_bounces_through_a_worker_and_back_ends_normally() {
    let _serial = serial().await;
    let Setup { server, worker, .. } = setup().await;
    tick().await;
    let echo = on("t.echo2", "server", "ping", method(None, None));
    let bouncer = on("t.bouncer", "session:S1", "bounce", method(None, None));
    server.install(&versioned_facet(&echo, 1, "ping", |_| Json::Null), Default::default()).await.unwrap();
    // The bounced call reaches this Echo object, held for the worker's identity. It stays alive, so the reload
    // gate must decide about the bounced root's lease.
    let held = server.hold(&echo, json!({}), GetOptions::principal(json!({"worker": "S1"}))).unwrap();
    held.ready().await.unwrap();
    let pinged = echo.clone();
    let factory = move |_: &Json, _: &Json, _: &mut Declare| {
        let pinged = pinged.clone();
        Ok(Implementation::variant("main").method("bounce", move |_, ctx| {
            let pinged = pinged.clone();
            async move {
                let echo = ctx.get(&pinged, json!({}), GetOptions::default()).await?;
                echo.call_json("ping", Json::Null, Some(&ctx)).await
            }
        }))
    };
    worker.serve(&bouncer, factory, ServeOptions::default()).unwrap();
    let target = server.hold(&bouncer, json!({}), GetOptions::default()).unwrap();
    let bounce = move |ctx: Ctx| async move { target.call_json("bounce", Json::Null, Some(&ctx)).await };
    server.root(bounce, RootOptions::default()).await.unwrap();
    sleep_ms(10).await;
    let options = ReloadOptions { gate_deadline: Some(ms(200)), ..ReloadOptions::default() };
    server.reload(vec![versioned_facet(&echo, 2, "ping", |_| Json::Null).into()], options).await.unwrap();
}
