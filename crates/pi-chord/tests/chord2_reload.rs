//! Port of upstream `test/reload.test.ts`, part 1: compatible reloads, recording mode, state versions and the
//! gate. The rest is in `chord2_reload_resources.rs` and `chord2_reload_contracts.rs`.

mod chord2_support;

use std::sync::Arc;

use chord2_support::reload::*;
use chord2_support::*;
use pi_chord::host2::declare::ResourceDeclaration;
use pi_chord::host2::{
    Declare, Implementation, Readable, ReloadOptions, RootOptions, StateEvent, StateOptions, define_facet, method,
};
use pi_chord::{ChordError, Json, json};

#[tokio::test]
async fn compatible_reload_keeps_pinned_roots_on_old_code_and_retires_the_previous() {
    let _serial = serial().await;
    let host = quiet();
    install(&host, greeter_facet(Spec::new("hello"))).await;
    let reference = hold_greeter(&host, json!({}));
    assert_eq!(greet(&reference, "a").await.unwrap(), "hello a");
    let go = Latch::default();
    let pinned = spawn_root(&host, {
        let (reference, go) = (reference.clone(), go.clone());
        move |ctx| async move {
            let first = greet_in(&reference, "p", &ctx).await?;
            go.wait().await;
            Ok(json!([first, greet_in(&reference, "p", &ctx).await?]))
        }
    });
    tick().await;
    let shapes: Log = Log::default();
    let seen = shapes.clone();
    let _shape = reference.on_shape(Arc::new(move |shape| seen.push(shape.contract.clone())));
    let report = host.reload(vec![greeter_facet(Spec::new("hi")).into()], ReloadOptions::default()).await.unwrap();
    assert!(report.gated.is_empty());
    assert_eq!(report.prepared.len(), 1);
    assert_eq!(greet(&reference, "b").await.unwrap(), "hi b");
    assert_eq!(shapes.len(), 1, "one shape push at the commit");
    assert_eq!(host.previous_code_version(), Some(report.previous));
    go.open(Json::Null);
    assert_eq!(pinned.await.unwrap().unwrap(), json!(["hello p", "hello p"]));
    assert_eq!(host.previous_code_version(), None);
    assert_eq!(state_of(&reference, "count"), json!({"n": 4}));
}

#[tokio::test]
async fn prepare_runs_in_recording_mode_and_a_failing_factory_changes_nothing() {
    let _serial = serial().await;
    let host = quiet();
    install(&host, greeter_facet(Spec::new("hello"))).await;
    let a = hold_greeter(&host, json!({"id": "a"}));
    let b = hold_greeter(&host, json!({"id": "b"}));
    a.ready().await.unwrap();
    b.ready().await.unwrap();
    let captured = host.root(|ctx| async move { Ok(ctx) }, RootOptions::default()).await.unwrap();
    let writing = Spec::new("hi").extra(move |decl: &mut Declare, _: &Json| {
        let probe = decl.state("probe", object(&[]), json!({}))?;
        probe.replace(&captured, json!({}))
    });
    let error = reload(&host, greeter_facet(writing)).await.unwrap_err();
    assert!(error.to_string().contains("read_only"), "{error}");

    let log: Log = Log::default();
    let opens = log.clone();
    let failing = Spec::new("hi").extra(move |decl: &mut Declare, input: &Json| {
        let id = text(&input["id"]).to_string();
        let (opened, closed, label) = (opens.clone(), opens.clone(), id.clone());
        let declaration = ResourceDeclaration::new(1, move |_| opened.push_ready(format!("open {label}")))
            .with_close(move |_: ()| closed.push_ready(format!("close {id}")));
        decl.resource("extra", declaration)?;
        if input["id"] == json!("b") {
            return Err(ChordError::error("b fails"));
        }
        Ok(())
    });
    let code_version = host.code_version();
    let error = reload(&host, greeter_facet(failing)).await.unwrap_err();
    // A plain error from the new factory surfaces as `unavailable`; the message says the reload aborted.
    assert!(is(&error, "unavailable"), "{error}");
    assert!(error.to_string().contains("reload aborted") && error.to_string().contains("b fails"), "{error}");
    assert_eq!(host.code_version(), code_version);
    assert_eq!(log.all(), ["open a", "close a"]);
    assert_eq!(greet(&a, "x").await.unwrap(), "hello x");
}

#[tokio::test]
async fn refuses_a_changed_cell_schema_without_a_version_bump() {
    let _serial = serial().await;
    let host = quiet();
    install(&host, greeter_facet(Spec::new("hello"))).await;
    hold_greeter(&host, json!({})).ready().await.unwrap();
    let changed = Spec::new("hi").schema(object(&[req("total", number())]), json!({"total": 0}));
    let error = reload(&host, greeter_facet(changed)).await.unwrap_err();
    assert!(is(&error, "invalid") && error.reason() == Some("state"), "{error}");
    assert!(error.to_string().contains("without a version bump"), "{error}");
}

#[tokio::test]
async fn a_state_bump_gates_migrates_resets_watchers_and_refuses_old_roots() {
    let _serial = serial().await;
    let host = quiet();
    install(&host, greeter_facet(Spec::new("hello"))).await;
    let reference = hold_greeter(&host, json!({}));
    greet(&reference, "a").await.unwrap();
    greet(&reference, "a").await.unwrap();
    let events: Log<Json> = Log::default();
    let seen = events.clone();
    let _events = state_events(&reference, "count", move |event: &StateEvent| seen.push(event_json(event)));
    let go = Latch::default();
    // Started before the reload, never touched the object: rejected after the commit.
    let late = spawn_root(&host, {
        let (reference, go) = (reference.clone(), go.clone());
        move |ctx| async move {
            go.wait().await;
            Ok(json!(greet_in(&reference, "late", &ctx).await?))
        }
    });
    let froms: Log<i64> = Log::default();
    let from_log = froms.clone();
    let v2 = Spec::new("hi")
        .schema(object(&[req("total", number())]), json!({"total": 0}))
        .bump(|draft| {
            let total = draft["total"].as_f64().unwrap_or_default();
            draft.set("total", total + 1.0)
        })
        .state(StateOptions {
            version: Some(2),
            migrate: Some(Arc::new(move |old: Json, from| {
                from_log.push(from);
                Ok(json!({"total": old["n"].clone()}))
            })),
        });
    let report = reload(&host, greeter_facet(v2)).await.unwrap();
    assert_eq!(report.gated.len(), 1);
    assert_eq!(froms.all(), [1]);
    assert_eq!(events.all().last(), Some(&json!({"type": "reset", "value": {"total": 2}})));
    assert_eq!(greet(&reference, "b").await.unwrap(), "hi b");
    assert_eq!(state_of(&reference, "count"), json!({"total": 3}));
    go.open(Json::Null);
    let error = late.await.unwrap().unwrap_err();
    assert!(is(&error, "upgrade_conflict") && error.retryable(), "{error}");
}

#[tokio::test]
async fn versions_are_per_cell_and_a_lowered_one_is_refused() {
    let _serial = serial().await;
    let host = quiet();
    let pair = local_type(
        "t.pair",
        object(&[]),
        vec![("a", unknown().into()), ("b", unknown().into()), ("bump", method(void(), void()).into())],
    );
    let migrated: Log = Log::default();
    let facet = |version: i64| {
        let (pair, migrated) = (pair.clone(), migrated.clone());
        define_facet("pair", move |api| {
            let migrated = migrated.clone();
            api.serve(
                &pair,
                move |_: &Json, _: &Json, decl: &mut Declare| {
                    let a = decl.state("a", number(), json!(1))?;
                    let migrated = migrated.clone();
                    let migrate = move |old: Json, from: i64| {
                        migrated.push(format!("b {from}->{version}"));
                        Ok(json!(old.as_f64().unwrap_or_default() * 10.0))
                    };
                    let options = StateOptions { version: Some(version), migrate: Some(Arc::new(migrate)) };
                    let b = decl.state_with("b", number(), pi_chord::host2::Initial::Value(json!(1)), options)?;
                    let bump = a.clone();
                    Ok(Implementation::new().state("a", a).state("b", b).method("bump", move |_, ctx| {
                        let a = bump.clone();
                        async move {
                            let next = a.value()?.as_f64().unwrap_or_default() + 1.0;
                            a.replace(&ctx, json!(next))?;
                            Ok(Json::Null)
                        }
                    }))
                },
                Default::default(),
            )
        })
    };
    install(&host, facet(1)).await;
    let reference = hold(&host, &pair, json!({}));
    reference.ready().await.unwrap();
    reference.call("bump", None, None).await.unwrap();
    let report = reload(&host, facet(2)).await.unwrap();
    assert_eq!(report.gated.len(), 1);
    assert_eq!(migrated.all(), ["b 1->2"]);
    let values = [state_of(&reference, "a"), state_of(&reference, "b")];
    assert_eq!(values, [json!(2), json!(10)], "a kept as it was, b migrated");
    let error = reload(&host, facet(1)).await.unwrap_err();
    assert!(error.to_string().contains("version of state b lowered from 2 to 1"), "{error}");
}

#[tokio::test]
async fn the_gate_refuses_late_old_roots_lets_grandfathered_ones_in_then_commits() {
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
            Ok(json!(greet_in(&reference, "g", &ctx).await?))
        }
    });
    tick().await;
    let v2 = Spec::new("hi").state(StateOptions { version: Some(2), migrate: None });
    let reloading = tokio::spawn(reload(&host, greeter_facet(v2)));
    until_conflict(&reference).await;
    go.open(Json::Null);
    assert_eq!(grandfathered.await.unwrap().unwrap(), json!("hello g"));
    let report = reloading.await.unwrap().unwrap();
    assert_eq!(report.gated.len(), 1);
    assert_eq!(greet(&reference, "n").await.unwrap(), "hi n");
}

#[tokio::test]
async fn a_missed_gate_deadline_aborts_reports_blocking_roots_and_changes_nothing() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    let facet = |greeting: &'static str, version: i64| {
        let log = log.clone();
        define_facet("greeter", move |api| {
            let log = log.clone();
            api.serve(
                &greeter(),
                move |_: &Json, _: &Json, decl: &mut Declare| {
                    let log = log.clone();
                    decl.task(move |ctx| {
                        let log = log.clone();
                        async move {
                            log.push(format!("start {greeting}"));
                            ctx.signal().cancelled().await;
                            log.push(format!("stop {greeting}"));
                            Ok(())
                        }
                    });
                    let options = StateOptions { version: Some(version), migrate: None };
                    let count =
                        decl.state_with("count", number(), pi_chord::host2::Initial::Value(json!(0)), options)?;
                    Ok(Implementation::new().state("count", count).method("greet", move |name, _| {
                        std::future::ready(Ok(json!(format!("{greeting} {}", text(&name)))))
                    }))
                },
                Default::default(),
            )
        })
    };
    install(&host, facet("hello", 1)).await;
    let reference = hold_greeter(&host, json!({}));
    reference.ready().await.unwrap();
    let go = Latch::default();
    let blocker = spawn_root(&host, {
        let (reference, go) = (reference.clone(), go.clone());
        move |ctx| async move {
            greet_in(&reference, "b", &ctx).await?;
            go.wait().await;
            Ok(Json::Null)
        }
    });
    tick().await;
    let options = ReloadOptions { gate_deadline: Some(ms(30)), ..ReloadOptions::default() };
    let error = host.reload(vec![facet("hi", 2).into()], options).await.unwrap_err();
    assert!(is(&error, "aborted") && error.reason() == Some("gate_deadline"), "{error}");
    let message = error.to_string();
    let blocking = message.split("blocking roots: ").nth(1).unwrap_or_default();
    assert!(blocking.starts_with("host#") && blocking.contains(':'), "{message}");
    assert_eq!(greet(&reference, "x").await.unwrap(), "hello x");
    until(|| log.len() >= 3).await;
    assert_eq!(log.all(), ["start hello", "stop hello", "start hello"]);
    go.open(Json::Null);
    blocker.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_reload_can_add_a_type_together_with_a_consumer_that_holds_it() {
    let _serial = serial().await;
    let host = quiet();
    install(&host, greeter_facet(Spec::new("hello"))).await;
    let reference = hold_greeter(&host, json!({}));
    reference.ready().await.unwrap();
    let clock = local_type("t.clock", object(&[]), vec![("now", method(void(), number()).into())]);
    let clock_facet = {
        let clock = clock.clone();
        define_facet("clock", move |api| {
            let now = || Ok(Implementation::new().method("now", |_, _| std::future::ready(Ok(json!(42)))));
            api.serve(&clock, move |_: &Json, _: &Json, _: &mut Declare| now(), Default::default())
        })
    };
    let consumer = |fail: bool| {
        let clock = clock.clone();
        define_facet("greeter", move |api| {
            let clock = clock.clone();
            api.serve(
                &greeter(),
                move |_: &Json, _: &Json, decl: &mut Declare| {
                    let clock = decl.get(&clock, json!({}), Default::default())?;
                    let count = decl.state("count", count_v1(), json!({"n": 0}))?;
                    if fail {
                        return Err(ChordError::error("consumer fails"));
                    }
                    Ok(Implementation::new().state("count", count).method("greet", move |name, ctx| {
                        let clock = clock.clone();
                        async move {
                            let now = clock.call_json("now", Json::Null, Some(&ctx)).await?;
                            Ok(json!(format!("{now} {}", text(&name))))
                        }
                    }))
                },
                Default::default(),
            )
        })
    };
    let code_version = host.code_version();
    let plugins = vec![clock_facet.clone().into(), consumer(true).into()];
    let error = host.reload(plugins, ReloadOptions::default()).await.unwrap_err();
    assert!(error.to_string().contains("consumer fails"), "{error}");
    assert_eq!(host.code_version(), code_version);
    assert_eq!(greet(&reference, "x").await.unwrap(), "hello x");
    let plugins = vec![clock_facet.into(), consumer(false).into()];
    host.reload(plugins, ReloadOptions::default()).await.unwrap();
    assert_eq!(greet(&reference, "x").await.unwrap(), "42 x");
}
