//! Port of upstream `test/review.test.ts`, reload cases: #1, #2, the reentrant commit observers, #4, #7 to #10 and
//! #11's argument stripping (#11's `fits` case is in `chord2_schema.rs`). Left out: "#1 a commit that still fails
//! midway", a white-box test that patches an object's `startTasks`; Rust objects cannot be patched.

mod chord2_support;

use std::future::ready;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chord2_support::reload::{ms, spawn_root, state_of};
use chord2_support::*;
use pi_chord::host2::declare::ResourceDeclaration;
use pi_chord::host2::{
    Contract, Declare, GetOptions, HostOptions, Implementation, Initial, Listenable, Readable, ReloadOptions,
    RemoteDef, ServeOptions, StateOptions, VariantDef, define_facet, host_named, method, remote,
};
use pi_chord::{BoxFuture, ChordError, ChordResult, Json, json};

fn item() -> Contract {
    local_type("t.item", object(&[req("id", string())]), vec![("version", method(None, string()).into())])
}

fn db() -> Contract {
    local_type("t.db", object(&[]), vec![("read", method(None, unknown()).into())])
}

fn on_host(name: &str, version: i64, main: VariantDef) -> Contract {
    let def = RemoteDef {
        version,
        input: object(&[]),
        host: host_named("host").unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote(name, &def).unwrap()
}

fn versioned(version: &'static str) -> Implementation {
    Implementation::new().method("version", move |_, _| ready(Ok(json!(version))))
}

async fn version(reference: &pi_chord::host2::Ref) -> String {
    text(&reference.call_json("version", Json::Null, None).await.unwrap()).to_string()
}

async fn ticks(n: usize) {
    for _ in 0..n {
        tick().await;
    }
}

/// A Rust `subscribe` cannot fail, so the listener's `select` fails instead (upstream's source throws).
#[tokio::test]
async fn r1_a_failing_listener_subscription_fails_prepare_and_changes_nothing() {
    let _serial = serial().await;
    let sources: Vec<Emitter> = (0..3).map(|_| Emitter::default()).collect();
    let facet = |version: &'static str, listen: bool| {
        let sources = sources.clone();
        define_facet("item", move |api| {
            let sources = sources.clone();
            let factory = move |input: &Json, _: &Json, decl: &mut Declare| {
                if listen {
                    let id = text(&input["id"]).to_string();
                    let index = ["a", "b", "c"].iter().position(|name| *name == id).unwrap();
                    let source = decl.resource("source", ResourceDeclaration::value(1, sources[index].clone()))?;
                    let select = move |emitter: &Emitter| match id.as_str() {
                        "b" => Err(ChordError::error("subscribe fails")),
                        _ => Ok(Listenable::Events(emitter.source())),
                    };
                    decl.on_select("events", &source, "subscribe", select, |_, _| ready(Ok(())))?;
                }
                Ok(versioned(version))
            };
            api.serve(&item(), factory, ServeOptions::default())
        })
    };
    let host = quiet();
    host.install(&facet("v1", false), Default::default()).await.unwrap();
    let refs: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|id| host.hold(&item(), json!({"id": id}), GetOptions::default()).unwrap())
        .collect();
    for reference in &refs {
        reference.ready().await.unwrap();
    }
    let code_version = host.code_version();
    let error = host.reload(vec![facet("v2", true).into()], ReloadOptions::default()).await.unwrap_err();
    assert!(error.to_string().contains("subscribe fails"), "{error}");
    assert_eq!(host.code_version(), code_version);
    for reference in &refs {
        assert_eq!(version(reference).await, "v1");
    }
    assert_eq!((sources[0].size(), sources[2].size()), (0, 0));
}

#[tokio::test]
async fn r2_migration_is_validated_against_the_new_remote_schemas_in_dev_mode() {
    let _serial = serial().await;
    let (n, total) = (object(&[req("n", number())]), object(&[req("total", number())]));
    let v1 = on_host("t.counted", 1, VariantDef::new().state("count", n.clone()));
    let v2 = on_host("t.counted", 2, VariantDef::new().state("count", total.clone()));
    let host = pi_chord::host2::create_host(HostOptions { dev: true, ..quiet_options(&Log::default()) });
    let linger = || ServeOptions::linger(ms(60_000));
    let served = v1.clone();
    let first_facet = define_facet("counted", move |api| {
        let n = n.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            Ok(Implementation::variant("main").state("count", decl.state("count", n.clone(), json!({"n": 3}))?))
        };
        api.serve(&served, factory, linger())
    });
    host.install(&first_facet, Default::default()).await.unwrap();
    let first = host.hold(&v1, json!({}), GetOptions::default()).unwrap();
    first.ready().await.unwrap();
    first.release();
    let served = v2.clone();
    let second_facet = define_facet("counted", move |api| {
        let total = total.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let migrate = Arc::new(|old: Json, _| Ok(json!({"total": old["n"].clone()})));
            let options = StateOptions { version: Some(2), migrate: Some(migrate) };
            let first = Initial::Value(json!({"total": 0}));
            Ok(Implementation::variant("main").state("count", decl.state_with("count", total.clone(), first, options)?))
        };
        api.serve(&served, factory, linger())
    });
    host.reload(vec![second_facet.into()], ReloadOptions::default()).await.unwrap();
    let second = host.hold(&v2, json!({}), GetOptions::default()).unwrap();
    second.ready().await.unwrap();
    ticks(10).await;
    assert_eq!(state_of(&second, "count"), json!({"total": 3}));
}

#[tokio::test]
async fn reentrant_observers_calling_another_object_during_a_commit_see_the_whole_new_code_version() {
    let _serial = serial().await;
    let other = local_type("t.other", object(&[]), vec![("version", method(None, string()).into())]);
    let facet = |version: &'static str| {
        let other = other.clone();
        define_facet("pair", move |api| {
            api.serve(&item(), move |_: &Json, _: &Json, _: &mut Declare| Ok(versioned(version)), Default::default())?;
            api.serve(&other, move |_: &Json, _: &Json, _: &mut Declare| Ok(versioned(version)), Default::default())
        })
    };
    let host = quiet();
    host.install(&facet("v1"), Default::default()).await.unwrap();
    let item_ref = host.hold(&item(), json!({"id": "a"}), GetOptions::default()).unwrap();
    let other_ref = host.hold(&other, json!({}), GetOptions::default()).unwrap();
    item_ref.ready().await.unwrap();
    other_ref.ready().await.unwrap();
    type Call = BoxFuture<ChordResult<Option<Json>>>;
    let seen: Arc<Mutex<Vec<Call>>> = Arc::default();
    let observed = seen.clone();
    // The call's root is admitted right in the observer, during the commit.
    let _shape =
        item_ref.on_shape(Arc::new(move |_| observed.lock().unwrap().push(other_ref.call("version", None, None))));
    host.reload(vec![facet("v2").into()], ReloadOptions::default()).await.unwrap();
    let calls: Vec<Call> = std::mem::take(&mut *seen.lock().unwrap());
    let versions: Vec<_> = futures::future::join_all(calls).await.into_iter().map(|call| call.unwrap()).collect();
    assert_eq!(versions, [Some(json!("v2"))]);
}

fn store() -> Contract {
    local_type("t.store", object(&[]), vec![("count", unknown().into())])
}

#[tokio::test]
async fn r4_a_running_listener_holds_a_lease_and_the_gate_waits_for_it_before_migrating() {
    let _serial = serial().await;
    let events = Emitter::default();
    let (go, seen) = (Latch::default(), Log::<Json>::default());
    let first = {
        let (events, go, seen) = (events.clone(), go.clone(), seen.clone());
        define_facet("store", move |api| {
            let (events, go, seen) = (events.clone(), go.clone(), seen.clone());
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let count = decl.state("count", object(&[req("n", number())]), json!({"n": 1}))?;
                let (go, seen, read) = (go.clone(), seen.clone(), count.clone());
                decl.on("slow", events.source(), move |_, _| {
                    let (go, seen, read) = (go.clone(), seen.clone(), read.clone());
                    async move {
                        go.wait().await;
                        seen.push(read.value()?);
                        Ok(())
                    }
                })?;
                Ok(Implementation::new().state("count", count))
            };
            api.serve(&store(), factory, Default::default())
        })
    };
    let host = quiet();
    host.install(&first, Default::default()).await.unwrap();
    let reference = host.hold(&store(), json!({}), GetOptions::default()).unwrap();
    reference.ready().await.unwrap();
    events.emit(json!(1));
    tick().await;
    let code_version = host.code_version();
    let second = define_facet("store", |api| {
        let factory = |_: &Json, _: &Json, decl: &mut Declare| {
            let migrate = Arc::new(|old: Json, _| Ok(json!({"total": old["n"].clone()})));
            let options = StateOptions { version: Some(2), migrate: Some(migrate) };
            let first = Initial::Value(json!({"total": 0}));
            let count = decl.state_with("count", object(&[req("total", number())]), first, options)?;
            Ok(Implementation::new().state("count", count))
        };
        api.serve(&store(), factory, Default::default())
    });
    let options = ReloadOptions { gate_deadline: Some(ms(2000)), ..ReloadOptions::default() };
    let reload = tokio::spawn(host.reload(vec![second.into()], options));
    ticks(20).await;
    assert_eq!(host.code_version(), code_version, "the gate must wait for the listener");
    go.open(Json::Null);
    reload.await.unwrap().unwrap();
    assert_eq!(seen.all(), [json!({"n": 1})]);
    assert_eq!(state_of(&reference, "count"), json!({"total": 1}));
}

#[tokio::test]
async fn r7_a_prepare_rerun_cannot_touch_retained_resources_even_open_ones() {
    let _serial = serial().await;
    let native = Arc::new(Mutex::new(false));
    let facet = |touch: bool| {
        let native = native.clone();
        define_facet("db", move |api| {
            let native = native.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let native = native.clone();
                let db = decl.resource("db", ResourceDeclaration::new(1, move |_| ready(Ok(native.clone()))))?;
                if touch {
                    *db.value()?.lock().unwrap() = true;
                }
                Ok(Implementation::new().method("read", move |_, _| {
                    ready(db.value().map(|native| json!({"mutated": *native.lock().unwrap()})))
                }))
            };
            api.serve(&db(), factory, Default::default())
        })
    };
    let host = quiet();
    host.install(&facet(false), Default::default()).await.unwrap();
    let reference = host.hold(&db(), json!({}), GetOptions::default()).unwrap();
    reference.ready().await.unwrap();
    let error = host.reload(vec![facet(true).into()], ReloadOptions::default()).await.unwrap_err();
    assert!(is(&error, "not_ready"), "{error}");
    assert!(!*native.lock().unwrap());
    assert_eq!(reference.call_json("read", Json::Null, None).await.unwrap(), json!({"mutated": false}));
}

#[tokio::test]
async fn r7_objects_of_types_a_reload_adds_start_listeners_and_tasks_only_at_commit() {
    let _serial = serial().await;
    let log: Log = Log::default();
    let fresh = local_type("t.fresh", object(&[]), vec![]);
    let host = quiet();
    let v1 = define_facet("item", |api| {
        api.serve(&item(), |_: &Json, _: &Json, _: &mut Declare| Ok(versioned("v1")), Default::default())
    });
    host.install(&v1, Default::default()).await.unwrap();
    let refs: Vec<_> =
        ["a", "b"].iter().map(|id| host.hold(&item(), json!({"id": id}), GetOptions::default()).unwrap()).collect();
    for reference in &refs {
        reference.ready().await.unwrap();
    }
    let plugins = |fail: bool| -> Vec<pi_chord::host2::PluginRef> {
        let (served, logged) = (fresh.clone(), log.clone());
        let fresh_facet = define_facet("fresh", move |api| {
            let logged = logged.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let logged = logged.clone();
                decl.task(move |_| logged.push_ready("task started"));
                Ok(Implementation::new())
            };
            api.serve(&served, factory, Default::default())
        });
        let fresh = fresh.clone();
        let item_facet = define_facet("item", move |api| {
            let fresh = fresh.clone();
            let factory = move |input: &Json, _: &Json, decl: &mut Declare| {
                decl.get(&fresh, json!({}), GetOptions::default())?;
                if fail && text(&input["id"]) == "b" {
                    return Err(ChordError::error("b fails"));
                }
                Ok(versioned("v2"))
            };
            api.serve(&item(), factory, Default::default())
        });
        vec![fresh_facet.into(), item_facet.into()]
    };
    let error = host.reload(plugins(true), ReloadOptions::default()).await.unwrap_err();
    assert!(error.to_string().contains("b fails"), "{error}");
    ticks(10).await;
    assert!(log.is_empty(), "{:?}", log.all());
    host.reload(plugins(false), ReloadOptions::default()).await.unwrap();
    ticks(10).await;
    assert_eq!(log.all(), ["task started"]);
}

#[tokio::test]
async fn r8_a_breaking_provider_change_and_its_updated_consumer_reload_in_one_batch() {
    let _serial = serial().await;
    let p1 = on_host("t.provider", 1, VariantDef::new().method("get", method(None, string())));
    let p2 = on_host("t.provider", 2, VariantDef::new().method("fetch", method(None, string())));
    let consumer = local_type("t.consumer", object(&[]), vec![("ask", method(None, string()).into())]);
    let both = |provider: &Contract, name: &'static str, answer: &'static str| {
        let (provider, consumer) = (provider.clone(), consumer.clone());
        define_facet("both", move |api| {
            let provider_factory = move |_: &Json, _: &Json, _: &mut Declare| {
                Ok(Implementation::variant("main").method(name, move |_, _| ready(Ok(json!(answer)))))
            };
            api.serve(&provider, provider_factory, Default::default())?;
            let provider = provider.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let target = decl.get(&provider, json!({}), GetOptions::default())?;
                Ok(Implementation::new().method("ask", move |_, ctx| {
                    let target = target.clone();
                    async move { target.call_json(name, Json::Null, Some(&ctx)).await }
                }))
            };
            api.serve(&consumer, factory, Default::default())
        })
    };
    let host = quiet();
    host.install(&both(&p1, "get", "old"), Default::default()).await.unwrap();
    let reference = host.hold(&consumer, json!({}), GetOptions::default()).unwrap();
    assert_eq!(reference.call_json("ask", Json::Null, None).await.unwrap(), json!("old"));
    host.reload(vec![both(&p2, "fetch", "new").into()], ReloadOptions::default()).await.unwrap();
    assert_eq!(reference.call_json("ask", Json::Null, None).await.unwrap(), json!("new"));
}

#[tokio::test]
async fn r9_an_exclusive_resource_whose_close_failed_is_restored_on_rollback_or_marked_unavailable() {
    let _serial = serial().await;
    for reopen_fails in [false, true] {
        let log: Log = Log::default();
        let opens = Arc::new(AtomicUsize::new(0));
        let facet = |rev: i64| {
            let (log, opens) = (log.clone(), opens.clone());
            define_facet("db", move |api| {
                let (log, opens) = (log.clone(), opens.clone());
                let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                    let (opened, closed, opens) = (log.clone(), log.clone(), opens.clone());
                    let open = move |_| {
                        if rev == 2 {
                            return ready(Err(ChordError::error("new port fails")));
                        }
                        if opens.fetch_add(1, Ordering::SeqCst) > 0 && reopen_fails {
                            return ready(Err(ChordError::error("reopen fails")));
                        }
                        opened.push(format!("open {rev}"));
                        ready(Ok(json!({"rev": rev})))
                    };
                    let close = move |_: Json| {
                        closed.push(format!("close {rev}"));
                        ready(Err(ChordError::error("close fails")))
                    };
                    let port =
                        decl.resource("port", ResourceDeclaration::new(rev, open).exclusive().with_close(close))?;
                    Ok(Implementation::new().method("read", move |_, _| ready(port.value())))
                };
                api.serve(&db(), factory, Default::default())
            })
        };
        let host = quiet();
        host.install(&facet(1), Default::default()).await.unwrap();
        let reference = host.hold(&db(), json!({}), GetOptions::default()).unwrap();
        reference.ready().await.unwrap();
        // The failing close aborts the reload at the gate; the old resource must not stay closed.
        let error = host.reload(vec![facet(2).into()], ReloadOptions::default()).await.unwrap_err();
        assert!(error.to_string().contains("close fails"), "{error}");
        let read = reference.call_json("read", Json::Null, None).await;
        if reopen_fails {
            let error = read.unwrap_err();
            assert!(is(&error, "unavailable"), "{error}");
        } else {
            assert_eq!(log.all(), ["open 1", "close 1", "open 1"]);
            assert_eq!(read.unwrap(), json!({"rev": 1}));
        }
    }
}

#[tokio::test]
async fn r10_retirement_closes_dropped_resources_before_releasing_dropped_holds() {
    let _serial = serial().await;
    let log: Log = Log::default();
    let dep = local_type("t.dep", object(&[]), vec![]);
    let facet = |with_decls: bool| {
        let (log, dep) = (log.clone(), dep.clone());
        define_facet("item", move |api| {
            let closed = log.clone();
            let dep_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let closed = closed.clone();
                let close = move |()| closed.push_ready("dep closed");
                decl.resource("r", ResourceDeclaration::value(1, ()).with_close(close))?;
                Ok(Implementation::new())
            };
            api.serve(&dep, dep_factory, Default::default())?;
            let (log, dep) = (log.clone(), dep.clone());
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                if with_decls {
                    decl.get(&dep, json!({}), GetOptions::default())?;
                    // Slow close: releasing the hold first would let the dependency close before it finishes.
                    let log = log.clone();
                    let close = move |()| {
                        let log = log.clone();
                        async move {
                            ticks(3).await;
                            log.push("own closed");
                            Ok(())
                        }
                    };
                    decl.resource("own", ResourceDeclaration::value(1, ()).with_close(close))?;
                }
                Ok(versioned(if with_decls { "v1" } else { "v2" }))
            };
            api.serve(&item(), factory, Default::default())
        })
    };
    let host = quiet();
    host.install(&facet(true), Default::default()).await.unwrap();
    let reference = host.hold(&item(), json!({"id": "a"}), GetOptions::default()).unwrap();
    reference.ready().await.unwrap();
    let go = Latch::default();
    let pinned = spawn_root(&host, {
        let (reference, go) = (reference.clone(), go.clone());
        move |ctx| async move {
            reference.call_json("version", Json::Null, Some(&ctx)).await?;
            go.wait().await;
            Ok(Json::Null)
        }
    });
    tick().await;
    host.reload(vec![facet(false).into()], ReloadOptions::default()).await.unwrap();
    assert!(log.is_empty(), "{:?}", log.all());
    go.open(Json::Null);
    pinned.await.unwrap().unwrap();
    until(|| log.len() == 2).await;
    assert_eq!(log.all(), ["own closed", "dep closed"]);
}

#[tokio::test]
async fn r11_arguments_are_stripped_to_what_the_consumer_knows_then_to_what_the_provider_knows() {
    let _serial = serial().await;
    let args = with(object(&[req("a", string())]), serde_json::json!({"additionalProperties": false}));
    let strict = on_host("t.strict", 1, VariantDef::new().method("echo", method(args, unknown())));
    let host = quiet();
    let factory = |_: &Json, _: &Json, _: &mut Declare| {
        Ok(Implementation::variant("main").method("echo", |args, _| ready(Ok(args))))
    };
    host.serve(&strict, factory, ServeOptions::default()).unwrap();
    let reference = host.hold(&strict, json!({}), GetOptions::default()).unwrap();
    assert_eq!(reference.call_json("echo", json!({"a": "x", "junk": 1}), None).await.unwrap(), json!({"a": "x"}));
}
