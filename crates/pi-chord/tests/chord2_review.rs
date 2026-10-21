//! Port of upstream `test/review.test.ts`, in-host cases: #5, both #6 cases, and the ctx races for an aborted
//! task ctx and for expired and foreign ctxs. Left out: the reload cases (#1, #2, #4, #7 to #11 and the commit
//! observers; phase 4) and the remote reference race (links, phase 3).

mod chord2_support;

use std::future::ready;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chord2_support::*;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::{
    Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation, PrincipalMatch, Ref, RefStatus,
    RegistryView, RootOptions, ServeOptions, create_host, define_registry, method,
};
use pi_chord::{ChordError, Json, json};

fn host() -> Host {
    create_host(HostOptions {
        principal: json!({"kind": "host"}),
        on_error: Some(Arc::new(|_| {})),
        ..HostOptions::default()
    })
}

async fn ticks(n: usize) {
    for _ in 0..n {
        tick().await;
    }
}

#[tokio::test]
async fn r5_disposal_waits_for_running_methods_before_closing_resources() {
    let _serial = serial().await;
    let worker = local_type("t.busy", object(&[req("id", string())]), vec![("work", method(None, boolean()).into())]);
    let log: Log = Log::default();
    let go: Arc<Mutex<Latch>> = Arc::default();
    let host = host();
    let (logged, gate) = (log.clone(), go.clone());
    let factory = move |input: &Json, _: &Json, decl: &mut Declare| {
        let id = text(&input["id"]).to_string();
        let (closing, label) = (logged.clone(), id.clone());
        let open = |_: OpenOptions| ready(Ok(Arc::new(Mutex::new(false))));
        let close = move |conn: Arc<Mutex<bool>>| {
            *conn.lock().unwrap() = true;
            closing.push(format!("close {label}"));
            ready(Ok(()))
        };
        let conn = decl.resource("conn", ResourceDeclaration::new(1, open).with_close(close))?;
        let (logged, gate) = (logged.clone(), gate.clone());
        Ok(Implementation::new().method("work", move |_, _| {
            let (opened, logged, conn, id) = (gate.lock().unwrap().wait(), logged.clone(), conn.clone(), id.clone());
            async move {
                opened.await;
                logged.push(format!("done {id}"));
                Ok(json!(*conn.value()?.lock().unwrap()))
            }
        }))
    };
    host.serve(&worker, factory, ServeOptions::default()).unwrap();
    let released = host.hold(&worker, json!({"id": "r"}), GetOptions::default()).unwrap();
    let call = tokio::spawn(released.call("work", None, None));
    ticks(10).await;
    released.release();
    ticks(10).await;
    go.lock().unwrap().open(Json::Null);
    assert_eq!(call.await.unwrap().unwrap(), Some(json!(false)));
    ticks(10).await;
    assert_eq!(log.all(), ["done r", "close r"]);

    *go.lock().unwrap() = Latch::default();
    log.clear();
    let forced = host.hold(&worker, json!({"id": "f"}), GetOptions::principal(json!({"user": "u"}))).unwrap();
    let forced_call = tokio::spawn(forced.call("work", None, None));
    ticks(10).await;
    let matching = PrincipalMatch::predicate(|principal| principal.get("user").and_then(Json::as_str) == Some("u"));
    let invalidated = tokio::spawn(host.invalidate_principal(matching));
    ticks(10).await;
    assert_eq!(forced.status(), RefStatus::Gone);
    assert!(log.is_empty(), "{:?}", log.all());
    go.lock().unwrap().open(Json::Null);
    invalidated.await.unwrap().unwrap();
    assert_eq!(forced_call.await.unwrap().unwrap(), Some(json!(false)));
    assert_eq!(log.all(), ["done f", "close f"]);
}

#[tokio::test]
async fn r6_a_registry_conflict_found_while_finalizing_activation_unwinds_the_loser_completely() {
    let _serial = serial().await;
    let errors = Log::default();
    let host = quiet_with(&errors);
    let routes = define_registry::<String>("t.review-routes", None);
    let log: Log = Log::default();
    for name in ["t.eager-a", "t.eager-b"] {
        let (routes, logged) = (routes.clone(), log.clone());
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let (opened, closed) = (logged.clone(), logged.clone());
            let open = move |_: OpenOptions| opened.push_ready(format!("open {name}"));
            let close = move |()| closed.push_ready(format!("close {name}"));
            decl.resource("r", ResourceDeclaration::new(1, open).with_close(close))?;
            decl.register(&routes, [("same".to_string(), name.to_string())])?;
            Ok(Implementation::new())
        };
        host.serve(&local_type(name, object(&[]), vec![]), factory, ServeOptions::host()).unwrap();
    }
    ticks(20).await;
    let count = |prefix: &str| log.all().iter().filter(|entry| entry.starts_with(prefix)).count();
    assert_eq!(count("open"), 2);
    assert_eq!(count("close"), 1);
    assert_eq!(host.registry(&routes).table(None).unwrap().len(), 1);
    let conflict = |error: &ChordError| is(error, "invalid") && error.reason() == Some("registry_conflict");
    assert!(errors.all().iter().any(conflict), "{:?}", errors.all());
}

fn store() -> Contract {
    local_type("t.store", object(&[]), vec![("count", unknown().into())])
}

#[tokio::test]
async fn r6_releasing_the_last_hold_while_listeners_start_up_does_not_resurrect_the_object() {
    let _serial = serial().await;
    let host = host();
    let log: Log = Log::default();
    let slot: Arc<Mutex<Option<Ref>>> = Arc::default();
    let runs = Arc::new(AtomicUsize::new(0));
    let (logged, held, counted) = (log.clone(), slot.clone(), runs.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        counted.fetch_add(1, Ordering::SeqCst);
        let count = decl.state("count", number(), json!(0))?;
        let (opened, closed) = (logged.clone(), logged.clone());
        let open = move |_: OpenOptions| opened.push_ready("open");
        let close = move |()| closed.push_ready("close");
        decl.resource("r", ResourceDeclaration::new(1, open).with_close(close))?;
        // A readable delivers its current value as the listener starts: release the only hold right then.
        let held = held.clone();
        decl.on("drop", &count, move |_, _| {
            if let Some(reference) = held.lock().unwrap().as_ref() {
                reference.release();
            }
            ready(Ok(()))
        })?;
        Ok(Implementation::new().state("count", count))
    };
    host.serve(&store(), factory, ServeOptions::default()).unwrap();
    let first = host.hold(&store(), json!({}), GetOptions::default()).unwrap();
    *slot.lock().unwrap() = Some(first.clone());
    let _ = first.ready().await;
    ticks(20).await;
    assert_eq!(log.all(), ["open", "close"]);
    let again = host.hold(&store(), json!({}), GetOptions::default()).unwrap();
    *slot.lock().unwrap() = None;
    again.ready().await.unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

fn target() -> Contract {
    local_type("t.target", object(&[]), vec![("hit", method(None, None).into())])
}

#[tokio::test]
async fn an_aborted_task_ctx_cannot_start_fresh_calls() {
    let _serial = serial().await;
    let host = host();
    let runner = local_type("t.runner", object(&[]), vec![]);
    let hits = Arc::new(AtomicUsize::new(0));
    let go = Latch::default();
    let outcome: Arc<Mutex<Option<ChordError>>> = Arc::default();
    let counted = hits.clone();
    let hit = move |_: &Json, _: &Json, _: &mut Declare| {
        let counted = counted.clone();
        Ok(Implementation::new().method("hit", move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            ready(Ok(Json::Null))
        }))
    };
    host.serve(&target(), hit, ServeOptions::default()).unwrap();
    let (gate, result) = (go.clone(), outcome.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let target = decl.get(&target(), json!({}), GetOptions::default())?;
        let (gate, result) = (gate.clone(), result.clone());
        decl.task(move |ctx: Ctx| {
            let (opened, target, result) = (gate.wait(), target.clone(), result.clone());
            async move {
                opened.await;
                *result.lock().unwrap() = target.call("hit", None, Some(&ctx)).await.err();
                Ok(())
            }
        });
        Ok(Implementation::new())
    };
    host.serve(&runner, factory, ServeOptions::default()).unwrap();
    let runner_ref = host.hold(&runner, json!({}), GetOptions::default()).unwrap();
    runner_ref.ready().await.unwrap();
    let target_ref = host.hold(&target(), json!({}), GetOptions::default()).unwrap();
    target_ref.ready().await.unwrap();
    runner_ref.release();
    ticks(10).await;
    go.open(Json::Null);
    ticks(20).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let outcome = outcome.lock().unwrap().clone();
    assert!(outcome.as_ref().is_some_and(|error| is(error, "aborted")), "{outcome:?}");
}

#[tokio::test]
async fn an_expired_ctx_and_a_foreign_ctx_are_refused() {
    let _serial = serial().await;
    let (host, other) = (host(), host());
    let hit =
        |_: &Json, _: &Json, _: &mut Declare| Ok(Implementation::new().method("hit", |_, _| ready(Ok(Json::Null))));
    host.serve(&target(), hit, ServeOptions::default()).unwrap();
    let target_ref = host.hold(&target(), json!({}), GetOptions::default()).unwrap();
    target_ref.ready().await.unwrap();
    let expired = host.root(|ctx: Ctx| ready(Ok(ctx)), RootOptions::default()).await.unwrap();
    let error = target_ref.call("hit", None, Some(&expired)).await.unwrap_err();
    assert!(is(&error, "aborted") && error.reason() == Some("expired"), "{error}");
    let refused = target_ref.clone();
    let foreign = other.root(
        move |ctx: Ctx| async move {
            let error = refused.call("hit", None, Some(&ctx)).await.unwrap_err();
            assert!(is(&error, "invalid") && error.reason() == Some("foreign_ctx"), "{error}");
            Ok(true)
        },
        RootOptions::default(),
    );
    assert!(foreign.await.unwrap());
}
