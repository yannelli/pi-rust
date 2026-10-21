//! Port of upstream `test/host.test.ts`, part 1: activation, lifetime, declarations and methods. The rest is in
//! `chord2_host_objects.rs`. Left out: async factories (Rust factories are synchronous).

mod chord2_support;

use std::future::ready;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chord2_support::*;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::{
    Contract, GetOptions, Host, Implementation, LoadOptions, Readable, ReadableRef, RefStatus, RootOptions,
    ServeOptions, define_facet, method,
};
use pi_chord::{ChordError, Json, json};
use tokio::sync::oneshot;

fn counter() -> Contract {
    local_type(
        "t.counter",
        object(&[req("name", string())]),
        vec![("count", object(&[req("n", number())]).into()), ("add", method(number(), number()).into())],
    )
}

fn serve_counter(host: &Host, log: &Log, options: ServeOptions) {
    let log = log.clone();
    let factory = move |input: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        let name = text(&input["name"]).to_string();
        log.push(format!("factory {name}"));
        let count = decl.state("count", object(&[req("n", number())]), json!({"n": 0}))?;
        let (opened, closed, label) = (log.clone(), log.clone(), name.clone());
        let open = move |_: OpenOptions| {
            opened.push(format!("open {label}"));
            ready(Ok(label.clone()))
        };
        let close = move |_: String| {
            closed.push(format!("close {name}"));
            ready(Ok(()))
        };
        decl.resource("conn", ResourceDeclaration::new(1, open).with_close(close))?;
        Ok(Implementation::new().state("count", count.clone()).method("add", move |delta, ctx| {
            let count = count.clone();
            async move {
                let delta = delta.as_f64().unwrap_or_default();
                count.change(&ctx, |draft| {
                    let n = draft["n"].as_f64().unwrap_or_default();
                    draft.set("n", n + delta)
                })?;
                Ok(count.value()?["n"].clone())
            }
        }))
    };
    host.serve(&counter(), factory, options).unwrap();
}

fn hold(host: &Host, contract: &Contract, input: Json) -> pi_chord::host2::Ref {
    host.hold(contract, input, GetOptions::default()).unwrap()
}

fn empty(name: &str) -> Contract {
    local_type(name, object(&[]), vec![])
}

#[tokio::test]
async fn activates_goes_live_binds_members_late_disposes_after_release() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    serve_counter(&host, &log, ServeOptions::default());
    let reference = hold(&host, &counter(), json!({"name": "a"}));
    assert_eq!(reference.status(), RefStatus::Loading);
    let error = reference.state("count").unwrap().value().unwrap_err();
    assert!(is(&error, "not_ready"), "{error}");
    let pending = tokio::spawn(reference.call("add", Some(json!(2)), None));
    reference.ready().await.unwrap();
    assert_eq!(reference.status(), RefStatus::Live);
    assert_eq!(pending.await.unwrap().unwrap(), Some(json!(2)));
    assert_eq!(reference.state("count").unwrap().value().unwrap(), json!({"n": 2}));
    reference.release();
    assert_eq!(reference.status(), RefStatus::Released);
    let error = reference.call("add", Some(json!(1)), None).await.unwrap_err();
    assert!(is(&error, "released"), "{error}");
    tick().await;
    assert_eq!(log.all(), ["factory a", "open a", "close a"]);
}

#[tokio::test]
async fn coalesces_concurrent_gets_and_keeps_the_object_while_held() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    serve_counter(&host, &log, ServeOptions::default());
    let a = hold(&host, &counter(), json!({"name": "x"}));
    let b = hold(&host, &counter(), json!({"name": "x"}));
    let (ready_a, ready_b) = futures::join!(a.ready(), b.ready());
    ready_a.unwrap();
    ready_b.unwrap();
    assert_eq!(log.all(), ["factory x", "open x"]);
    a.release();
    tick().await;
    assert_eq!(b.call_json("add", json!(1), None).await.unwrap(), json!(1));
    b.release();
    tick().await;
    assert_eq!(log.all(), ["factory x", "open x", "close x"]);
}

#[tokio::test]
async fn lingers_after_the_last_release_and_revives_on_a_new_get() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    serve_counter(&host, &log, ServeOptions::linger(Duration::from_millis(30)));
    let a = hold(&host, &counter(), json!({"name": "l"}));
    a.call("add", Some(json!(5)), None).await.unwrap();
    a.release();
    let b = hold(&host, &counter(), json!({"name": "l"}));
    b.ready().await.unwrap();
    assert_eq!(b.state("count").unwrap().value().unwrap(), json!({"n": 5}));
    b.release();
    sleep_ms(60).await;
    assert_eq!(log.all(), ["factory l", "open l", "close l"]);
}

#[tokio::test]
async fn disposes_an_abandoned_activation_once_it_settles() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    let gate: Arc<Mutex<Option<oneshot::Sender<String>>>> = Arc::default();
    let slow = empty("t.slow");
    let (factory_log, factory_gate) = (log.clone(), gate.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        let (aborts, gate, closes) = (factory_log.clone(), factory_gate.clone(), factory_log.clone());
        let open = move |options: OpenOptions| {
            let (sender, receiver) = oneshot::channel();
            *gate.lock().unwrap() = Some(sender);
            let aborts = aborts.clone();
            options.signal.add_listener(move |_| aborts.push("abort seen"));
            async move { receiver.await.map_err(|_| ChordError::error("never released")) }
        };
        let close = move |_: String| {
            closes.push("closed");
            ready(Ok(()))
        };
        decl.resource("r", ResourceDeclaration::new(1, open).with_close(close))?;
        Ok(Implementation::new())
    };
    host.serve(&slow, factory, ServeOptions::default()).unwrap();
    let release = || {
        let sender = gate.lock().unwrap().take();
        let _ = sender.expect("open was called").send("r".into());
    };
    let reference = hold(&host, &slow, json!({}));
    tick().await;
    reference.release();
    tick().await;
    release();
    tick().await;
    tick().await;
    assert_eq!(log.all(), ["abort seen", "closed"]);
    let again = hold(&host, &slow, json!({}));
    tick().await;
    release();
    again.ready().await.unwrap();
    assert_eq!(again.status(), RefStatus::Live);
}

#[tokio::test]
async fn propagates_activation_failures_to_dependents_and_retries_on_the_next_get() {
    let _serial = serial().await;
    let host = quiet();
    let fail = Arc::new(AtomicBool::new(true));
    let (leaf, parent) = (empty("t.leaf"), empty("t.parent"));
    let failing = fail.clone();
    let leaf_factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        let failing = failing.clone();
        let open = move |_: OpenOptions| {
            ready(if failing.load(Ordering::SeqCst) { Err(ChordError::error("boom")) } else { Ok(1) })
        };
        decl.resource("r", ResourceDeclaration::new(1, open))?;
        Ok(Implementation::new())
    };
    host.serve(&leaf, leaf_factory, ServeOptions::default()).unwrap();
    let held = leaf.clone();
    let parent_factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        decl.get(&held, json!({}), GetOptions::default())?;
        Ok(Implementation::new())
    };
    host.serve(&parent, parent_factory, ServeOptions::default()).unwrap();
    let first = hold(&host, &parent, json!({}));
    // A plain error from a dependency surfaces as `unavailable`, with the original as cause.
    let error = first.ready().await.unwrap_err();
    assert!(is(&error, "unavailable") && error.to_string().contains("boom"), "{error}");
    assert_eq!(first.status(), RefStatus::Gone);
    fail.store(false, Ordering::SeqCst);
    let retry = hold(&host, &parent, json!({}));
    retry.ready().await.unwrap();
    assert_eq!(retry.status(), RefStatus::Live);
}

#[tokio::test]
async fn unwinds_declarations_when_a_factory_fails() {
    let _serial = serial().await;
    let host = quiet();
    let (leaf, bad) = (empty("t.leaf2"), empty("t.bad"));
    let live = Arc::new(AtomicI64::new(0));
    let counted = live.clone();
    let leaf_factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        let (opens, closes) = (counted.clone(), counted.clone());
        let open = move |_: OpenOptions| ready(Ok(opens.fetch_add(1, Ordering::SeqCst)));
        let close = move |_: i64| {
            closes.fetch_sub(1, Ordering::SeqCst);
            ready(Ok(()))
        };
        decl.resource("r", ResourceDeclaration::new(1, open).with_close(close))?;
        Ok(Implementation::new())
    };
    host.serve(&leaf, leaf_factory, ServeOptions::default()).unwrap();
    let held = leaf.clone();
    let bad_factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        decl.get(&held, json!({}), GetOptions::default())?;
        Err(ChordError::error("factory failed"))
    };
    host.serve(&bad, bad_factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &bad, json!({})).ready().await.unwrap_err();
    assert!(error.to_string().contains("factory failed"), "{error}");
    tick().await;
    tick().await;
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejects_duplicate_declaration_names() {
    let _serial = serial().await;
    let host = quiet();
    let dup = empty("t.dup");
    let factory = |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        decl.state("a", object(&[]), json!({}))?;
        decl.state("a", object(&[]), json!({}))?;
        Ok(Implementation::new())
    };
    host.serve(&dup, factory, ServeOptions::default()).unwrap();
    let error = hold(&host, &dup, json!({})).ready().await.unwrap_err();
    assert!(error.to_string().contains("duplicate state name a"), "{error}");
}

#[tokio::test]
async fn detects_hold_cycles() {
    let _serial = serial().await;
    let host = quiet();
    let (a, b) = (empty("t.cycle-a"), empty("t.cycle-b"));
    for (contract, target) in [(a.clone(), b.clone()), (b, a.clone())] {
        let factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
            decl.get(&target, json!({}), GetOptions::default())?;
            Ok(Implementation::new())
        };
        host.serve(&contract, factory, ServeOptions::default()).unwrap();
    }
    let error = hold(&host, &a, json!({})).ready().await.unwrap_err();
    assert!(error.to_string().contains("cycle"), "{error}");
}

#[tokio::test]
async fn closes_resources_in_reverse_order_runs_all_steps_and_aggregates_errors() {
    let _serial = serial().await;
    let errors = Log::default();
    let host = quiet_with(&errors);
    let log: Log = Log::default();
    let multi = empty("t.multi");
    let closing = log.clone();
    let factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        let (la, lb, lc) = (closing.clone(), closing.clone(), closing.clone());
        let a = decl.resource(
            "a",
            ResourceDeclaration::value(1, "a".to_string()).with_close(move |_| {
                la.push("close a");
                ready(Ok(()))
            }),
        )?;
        let b = ResourceDeclaration::value(1, "b".to_string()).with_deps(vec![(&a).into()]).with_close(move |_| {
            lb.push("close b");
            ready(Err(ChordError::error("b failed")))
        });
        decl.resource("b", b)?;
        let c = ResourceDeclaration::value(1, "c".to_string()).with_close(move |_| {
            lc.push("close c");
            ready(Err(ChordError::error("c failed")))
        });
        decl.resource("c", c)?;
        Ok(Implementation::new())
    };
    host.serve(&multi, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &multi, json!({}));
    reference.ready().await.unwrap();
    reference.release();
    tick().await;
    tick().await;
    assert_eq!(log.all().last().map(String::as_str), Some("close a"));
    assert_eq!(log.len(), 3);
    match errors.all().first() {
        Some(ChordError::Aggregate { errors, .. }) => assert_eq!(errors.len(), 2),
        other => panic!("expected an aggregate error, got {other:?}"),
    }
}

#[tokio::test]
async fn releases_ctx_get_holds_when_the_call_settles() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    serve_counter(&host, &log, ServeOptions::default());
    let run = |ctx: pi_chord::host2::Ctx| async move {
        let counter = ctx.get(&counter(), json!({"name": "c"}), GetOptions::default()).await?;
        counter.call_json("add", json!(3), Some(&ctx)).await
    };
    let value = host.root(run, RootOptions::default()).await.unwrap();
    assert_eq!(value, json!(3));
    tick().await;
    assert_eq!(log.all(), ["factory c", "open c", "close c"]);
}

#[tokio::test]
async fn refuses_a_second_declaration_of_one_name_per_kind() {
    let _serial = serial().await;
    let host = quiet();
    let twice = local_type("t.twice", object(&[req("kind", string())]), vec![]);
    let factory = |input: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
        let open = || ResourceDeclaration::value(1, Json::Null);
        decl.state("x", number(), json!(0))?;
        decl.resource("x", open())?;
        let y = decl.state("y", number(), json!(0))?;
        decl.on("x", &y, |_, _| async { Ok(()) })?;
        match text(&input["kind"]) {
            "state" => drop(decl.state("x", number(), json!(1))?),
            "resource" => drop(decl.resource("x", open())?),
            "listener" => {
                let z = decl.state("z", number(), json!(0))?;
                decl.on("x", &z, |_, _| async { Ok(()) })?;
            }
            _ => {}
        }
        Ok(Implementation::new())
    };
    host.serve(&twice, factory, ServeOptions::default()).unwrap();
    hold(&host, &twice, json!({"kind": "none"})).ready().await.unwrap();
    for kind in ["state", "resource", "listener"] {
        let error = hold(&host, &twice, json!({"kind": kind})).ready().await.unwrap_err();
        assert!(is(&error, "invalid") && error.reason() == Some("duplicate"), "{error}");
        assert!(error.to_string().contains(&format!("{kind} name x")), "{error}");
    }
}

#[tokio::test]
async fn methods_without_arguments_get_null_and_void_methods_return_nothing() {
    let _serial = serial().await;
    let host = quiet();
    let tool = local_type(
        "t.tool",
        object(&[]),
        vec![
            ("ping", method(None, None).into()),
            ("count", method(None, number()).into()),
            ("note", method(string(), None).into()),
        ],
    );
    let seen: Log = Log::default();
    let recorded = seen.clone();
    let factory = move |_: &Json, _: &Json, _: &mut pi_chord::host2::Declare| {
        let (pings, notes) = (recorded.clone(), recorded.clone());
        Ok(Implementation::new()
            .method("ping", move |args, ctx| {
                pings.push(format!("ping {args} {}", !ctx.root_id().is_empty()));
                ready(Ok(json!("ignored")))
            })
            .method("count", |_, _| ready(Ok(json!(3))))
            .method("note", move |text, ctx| {
                notes.push(format!("note {} {}", text.as_str().unwrap_or_default(), !ctx.root_id().is_empty()));
                ready(Ok(Json::Null))
            }))
    };
    host.serve(&tool, factory, ServeOptions::default()).unwrap();
    let reference = hold(&host, &tool, json!({}));
    reference.ready().await.unwrap();
    assert_eq!(reference.call("ping", None, None).await.unwrap(), None);
    let counted = reference.clone();
    let run = move |ctx: pi_chord::host2::Ctx| async move { counted.call("count", None, Some(&ctx)).await };
    assert_eq!(host.root(run, RootOptions::default()).await.unwrap(), Some(json!(3)));
    assert_eq!(reference.call("note", Some(json!("hi")), None).await.unwrap(), None);
    assert_eq!(seen.all(), ["ping null true", "note hi true"]);
}

#[tokio::test]
async fn a_listener_on_state_gets_the_current_value_at_start_and_again_after_a_reload() {
    let _serial = serial().await;
    let host = quiet();
    let doubled = local_type("t.doubled", object(&[]), vec![("set", method(number(), None).into())]);
    let seen: Log<Json> = Log::default();
    let facet = || {
        let (served, listened) = (doubled.clone(), seen.clone());
        define_facet("doubled", move |api| {
            let listened = listened.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut pi_chord::host2::Declare| {
                let n = decl.state("n", number(), json!(1))?;
                let twice = |values: &[Json]| Ok(json!(values[0].as_f64().unwrap_or_default() * 2.0));
                let derived = decl.derive(vec![n.clone() as ReadableRef], twice);
                let listened = listened.clone();
                decl.on("doubled", &derived, move |value, _| {
                    listened.push(value);
                    ready(Ok(()))
                })?;
                Ok(Implementation::new()
                    .method("set", move |value, ctx| ready(n.replace(&ctx, value).map(|()| Json::Null))))
            };
            api.serve(&served, factory, ServeOptions::host())
        })
    };
    host.install(&facet(), LoadOptions::default()).await.unwrap();
    until(|| seen.len() == 1).await;
    let reference = hold(&host, &doubled, json!({}));
    let run = move |ctx: pi_chord::host2::Ctx| async move { reference.call("set", Some(json!(5)), Some(&ctx)).await };
    host.root(run, RootOptions::default()).await.unwrap();
    until(|| seen.len() == 2).await;
    assert_eq!(seen.all(), [json!(2), json!(10)]);
    // The reload makes a new derivation: the listener subscribes again and gets its current value once.
    host.reload(vec![facet().into()], Default::default()).await.unwrap();
    until(|| seen.len() == 3).await;
    tick().await;
    assert_eq!(seen.all(), [json!(2), json!(10), json!(10)]);
}
