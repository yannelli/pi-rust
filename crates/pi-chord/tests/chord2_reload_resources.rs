//! Port of upstream `test/reload.test.ts`, part 2: resources across reloads, the two-version rule, forced
//! retirement, task replacement and facet signals. Upstream's fake timers are real short timers here.

mod chord2_support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chord2_support::reload::*;
use chord2_support::*;
use pi_chord::context::AbortSignal;
use pi_chord::host2::declare::ResourceDeclaration;
use pi_chord::host2::{
    Declare, Facet, HostOptions, Implementation, ReloadOptions, RootOptions, create_host, define_facet,
};
use pi_chord::{ChordError, Json, json};

/// A greeter whose factory declares through `declare`, which returns the greet function.
fn custom(declare: impl Fn(&mut Declare) -> Result<Greet, ChordError> + Send + Sync + 'static) -> Facet {
    let declare = Arc::new(declare);
    define_facet("greeter", move |api| {
        let declare = declare.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let greet = declare(decl)?;
            let count = decl.state("count", count_v1(), json!({"n": 0}))?;
            Ok(Implementation::new()
                .state("count", count)
                .method("greet", move |name, ctx| greet(text(&name).into(), ctx)))
        };
        api.serve(&greeter(), factory, Default::default())
    })
}

type Greet =
    Arc<dyn Fn(String, pi_chord::host2::Ctx) -> pi_chord::BoxFuture<pi_chord::ChordResult<Json>> + Send + Sync>;

#[tokio::test]
async fn exclusive_resources_are_replaced_at_the_gate_and_others_at_prepare() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    let facet = |rev: i64| {
        let log = log.clone();
        custom(move |decl| {
            let (opens, closes) = (log.clone(), log.clone());
            let open = move |_| {
                opens.push(format!("open lock {rev}"));
                std::future::ready(Ok(rev))
            };
            let close = move |_: i64| closes.push_ready(format!("close lock {rev}"));
            let lock = decl.resource("lock", ResourceDeclaration::new(rev, open).exclusive().with_close(close))?;
            let (opens, closes) = (log.clone(), log.clone());
            let cache = ResourceDeclaration::new(rev, move |_| opens.push_ready(format!("open cache {rev}")))
                .with_close(move |_: ()| closes.push_ready(format!("close cache {rev}")));
            decl.resource("cache", cache)?;
            Ok(Arc::new(move |name, _| {
                let value = lock.value().map(|rev| json!(format!("{rev} {name}")));
                Box::pin(std::future::ready(value))
            }))
        })
    };
    install(&host, facet(1)).await;
    let reference = hold_greeter(&host, json!({}));
    assert_eq!(greet(&reference, "x").await.unwrap(), "1 x");
    log.clear();
    let report = reload(&host, facet(2)).await.unwrap();
    assert_eq!(report.gated.len(), 1);
    assert_eq!(greet(&reference, "x").await.unwrap(), "2 x");
    tick().await;
    assert_eq!(log.all(), ["open cache 2", "close lock 1", "open lock 2", "close cache 1"]);
}

#[tokio::test]
async fn the_next_reload_retires_the_previous_code_version_and_supersedes_its_roots() {
    let _serial = serial().await;
    let host = quiet();
    install(&host, greeter_facet(Spec::new("v1"))).await;
    let reference = hold_greeter(&host, json!({}));
    reference.ready().await.unwrap();
    let first = host.code_version();
    let go = Latch::default();
    let pinned = spawn_root(&host, {
        let (reference, go) = (reference.clone(), go.clone());
        move |ctx| async move {
            greet_in(&reference, "p", &ctx).await?;
            go.wait().await;
            Ok(json!(greet_in(&reference, "p", &ctx).await?))
        }
    });
    tick().await;
    reload(&host, greeter_facet(Spec::new("v2"))).await.unwrap();
    assert_eq!(host.previous_code_version(), Some(first));
    let options = ReloadOptions { retire_deadline: Some(ms(20)), ..ReloadOptions::default() };
    host.reload(vec![greeter_facet(Spec::new("v3")).into()], options).await.unwrap();
    go.open(Json::Null);
    let error = pinned.await.unwrap().unwrap_err();
    assert!(is(&error, "superseded") && error.retryable(), "{error}");
    let options = RootOptions { code_version: Some(first), ..RootOptions::default() };
    let error = host.root(|_| async { Ok(1) }, options).await.unwrap_err();
    assert!(is(&error, "superseded"), "{error}");
    assert_eq!(greet(&reference, "n").await.unwrap(), "v3 n");
}

#[tokio::test]
async fn forced_retirement_aborts_leftover_roots_before_closing_what_only_their_code_used() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    let hold = Latch::default();
    let facet = |version: i64| {
        let (log, hold) = (log.clone(), hold.clone());
        custom(move |decl| {
            let closes = log.clone();
            let declaration = ResourceDeclaration::value(version, version)
                .with_close(move |_: i64| closes.push_ready(format!("close {version}")));
            let conn = decl.resource("conn", declaration)?;
            let (log, hold) = (log.clone(), hold.clone());
            Ok(Arc::new(move |name: String, ctx: pi_chord::host2::Ctx| {
                let (conn, log, hold) = (conn.clone(), log.clone(), hold.clone());
                Box::pin(async move {
                    if name == "hold" {
                        hold.wait().await;
                        let aborted = ctx.signal().aborted();
                        return Ok(json!(format!("held on {}, aborted {aborted}", conn.value()?)));
                    }
                    if name != "wait" {
                        return Ok(json!(name));
                    }
                    ctx.signal().cancelled().await;
                    // Still open: the abort comes first, then the grace, then the close.
                    log.push(format!("aborted, conn {}", conn.value()?));
                    Ok(json!(name))
                })
            }))
        })
    };
    install(&host, facet(1)).await;
    let reference = hold_greeter(&host, json!({}));
    reference.ready().await.unwrap();
    let old = spawn_root(&host, {
        let reference = reference.clone();
        move |ctx| async move { Ok(json!(greet_in(&reference, "wait", &ctx).await?)) }
    });
    tick().await;
    reload(&host, facet(2)).await.unwrap();
    // A root of the code version that becomes the older one: retiring the oldest leaves it alone.
    let current = spawn_root(&host, {
        let reference = reference.clone();
        move |ctx| async move { Ok(json!(greet_in(&reference, "hold", &ctx).await?)) }
    });
    tick().await;
    let options = ReloadOptions { retire_deadline: Some(ms(20)), ..ReloadOptions::default() };
    host.reload(vec![facet(3).into()], options).await.unwrap();
    assert_eq!(old.await.unwrap().unwrap(), json!("wait"));
    until(|| log.all().contains(&"close 1".to_string())).await;
    assert_eq!(log.all(), ["aborted, conn 1", "close 1"]);
    hold.open(Json::Null);
    assert_eq!(current.await.unwrap().unwrap(), json!("held on 2, aborted false"));
    assert_eq!(greet(&reference, "x").await.unwrap(), "x");
}

#[tokio::test]
async fn a_kept_resource_closes_with_the_newest_declarations_close() {
    let _serial = serial().await;
    let host = quiet();
    let log: Log = Log::default();
    let facet = |label: &'static str| {
        let log = log.clone();
        custom(move |decl| {
            let closes = log.clone();
            let declaration = ResourceDeclaration::value(1, ())
                .with_close(move |_: ()| closes.push_ready(format!("close by {label}")));
            decl.resource("conn", declaration)?;
            Ok(Arc::new(move |name, _| Box::pin(std::future::ready(Ok(json!(format!("{label} {name}")))))))
        })
    };
    install(&host, facet("a")).await;
    let reference = hold_greeter(&host, json!({}));
    reference.ready().await.unwrap();
    reload(&host, facet("b")).await.unwrap();
    reload(&host, facet("c")).await.unwrap();
    assert_eq!(greet(&reference, "x").await.unwrap(), "c x");
    assert!(log.is_empty(), "kept, not reopened");
    reference.release();
    until(|| !log.is_empty()).await;
    assert_eq!(log.all(), ["close by c"]);
}

#[tokio::test]
async fn a_task_that_ignores_its_abort_delays_its_replacement_at_most_the_task_stop_deadline() {
    let _serial = serial().await;
    let errors: Log<ChordError> = Log::default();
    let host = create_host(HostOptions { task_stop_deadline: ms(30), ..quiet_options(&errors) });
    let log: Log = Log::default();
    let stuck = Latch::default();
    let facet = |label: &'static str| {
        let (log, stuck) = (log.clone(), stuck.clone());
        custom(move |decl| {
            let (log, stuck) = (log.clone(), stuck.clone());
            decl.task(move |_| {
                let (log, stuck) = (log.clone(), stuck.clone());
                async move {
                    log.push(format!("start {label}"));
                    if label == "a" {
                        stuck.wait().await; // ignores ctx.signal
                    }
                    Ok(())
                }
            });
            Ok(Arc::new(|name, _| Box::pin(std::future::ready(Ok(json!(name))))))
        })
    };
    install(&host, facet("a")).await;
    let reference = hold_greeter(&host, json!({}));
    reference.ready().await.unwrap();
    reload(&host, facet("b")).await.unwrap();
    sleep_ms(10).await;
    assert_eq!(log.all(), ["start a"], "waits for the old run");
    assert!(errors.is_empty());
    sleep_ms(60).await;
    assert_eq!(log.all(), ["start a", "start b"]);
    let reported = errors.all().iter().any(|e| is(e, "aborted") && e.to_string().contains("still running after 30 ms"));
    assert!(reported, "{:?}", errors.all());
    stuck.open(Json::Null);
}

#[tokio::test]
async fn a_facet_signal_aborts_when_replaced_removed_aborted_or_the_host_closes() {
    let _serial = serial().await;
    let host = quiet();
    let signals: Arc<Mutex<HashMap<String, AbortSignal>>> = Arc::default();
    let aborted = |name: &str| signals.lock().unwrap().get(name).map(AbortSignal::aborted);
    let facet = |label: &'static str, fail: bool| {
        let signals = signals.clone();
        define_facet("greeter", move |api| {
            signals.lock().unwrap().insert(label.into(), api.signal().clone());
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                if fail {
                    return Err(ChordError::error("broken"));
                }
                let count = decl.state("count", count_v1(), json!({"n": 0}))?;
                Ok(Implementation::new().state("count", count).method("greet", |name, _| std::future::ready(Ok(name))))
            };
            api.serve(&greeter(), factory, Default::default())
        })
    };
    let other = {
        let signals = signals.clone();
        define_facet("other", move |api| {
            signals.lock().unwrap().insert("other".into(), api.signal().clone());
            Ok(())
        })
    };
    install(&host, facet("a", false)).await;
    install(&host, other).await;
    hold_greeter(&host, json!({})).ready().await.unwrap();
    reload(&host, facet("b", true)).await.unwrap_err();
    assert_eq!(aborted("b"), Some(true), "the aborted reload's run");
    assert_eq!(aborted("a"), Some(false), "still installed");
    reload(&host, facet("c", false)).await.unwrap();
    assert_eq!(aborted("a"), Some(true), "replaced");
    assert_eq!(aborted("c"), Some(false));
    let options = ReloadOptions { remove: vec!["other".into()], ..ReloadOptions::default() };
    host.reload(vec![], options).await.unwrap();
    assert_eq!(aborted("other"), Some(true), "removed");
    host.close().await.unwrap();
    assert_eq!(aborted("c"), Some(true), "host closed");
}
