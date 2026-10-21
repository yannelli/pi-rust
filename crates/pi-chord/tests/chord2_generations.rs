//! Port of upstream `test/generations.test.ts`: old code generations and disposed objects are dropped. A
//! generation's facet closes over a sentinel; upstream watches it with a `FinalizationRegistry` after a forced GC,
//! here a `Weak` shows whether anything still holds it (reference counts drop at once, so no GC is needed).

mod chord2_support;

use std::sync::{Arc, Mutex, Weak};

use chord2_support::*;
use pi_chord::host2::{
    Cell, Contract, Declare, Facet, Host, Implementation, LoadOptions, RegistryView, ReloadOptions, RootOptions,
    ServeOptions, define_facet, define_registry, method,
};
use pi_chord::{Json, json};

type Sentinels = Vec<(i64, Weak<i64>)>;

/// Tracks which sentinels are still held.
#[derive(Clone, Default)]
struct Tracker(Arc<Mutex<Sentinels>>);

impl Tracker {
    fn track(&self, id: i64) -> Arc<i64> {
        let sentinel = Arc::new(id);
        self.0.lock().unwrap().push((id, Arc::downgrade(&sentinel)));
        sentinel
    }

    fn alive(&self) -> Vec<i64> {
        let tracked = self.0.lock().unwrap();
        let mut alive: Vec<i64> =
            tracked.iter().filter(|(_, weak)| weak.strong_count() > 0).map(|(id, _)| *id).collect();
        alive.sort();
        alive
    }

    /// Waits until only `expected` is held (spawned work may still drop its share), at most a second.
    async fn collect(&self, expected: &[i64]) -> Vec<i64> {
        for _ in 0..100 {
            if self.alive() == expected {
                break;
            }
            sleep_ms(10).await;
        }
        self.alive()
    }
}

const RELOADS: i64 = 6;

fn store() -> Contract {
    local_type("t.gen-store", object(&[]), vec![("get", method(None, number()).into()), ("count", number().into())])
}

type Declares = Arc<dyn Fn(&mut Declare, &Arc<Cell>, Arc<i64>) -> pi_chord::ChordResult<()> + Send + Sync>;

/// A generation of the store facet: its factory and `get` close over the generation's sentinel.
fn generation(tracker: &Tracker, id: i64, options: ServeOptions, declare: Declares) -> Facet {
    let sentinel = tracker.track(id);
    define_facet("store", move |api| {
        let (sentinel, declare) = (sentinel.clone(), declare.clone());
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let count = decl.state("count", number(), json!(0))?;
            declare(decl, &count, sentinel.clone())?;
            let sentinel = sentinel.clone();
            Ok(Implementation::new().state("count", count).method("get", move |_, ctx| {
                let sentinel = sentinel.clone();
                // An abort listener on the call's signal must not outlive the call.
                let held = sentinel.clone();
                ctx.signal().add_listener(move |_| drop(held));
                std::future::ready(Ok(json!(*sentinel)))
            }))
        };
        api.serve(&store(), factory, options.clone())
    })
}

async fn reload(host: &Host, facet: Facet) {
    host.reload(vec![facet.into()], ReloadOptions::default()).await.unwrap();
}

async fn get_in_root(host: &Host) -> Json {
    let run = |ctx: pi_chord::host2::Ctx| async move {
        let store = ctx.get(&store(), json!({}), Default::default()).await?;
        store.call_json("get", Json::Null, Some(&ctx)).await
    };
    host.root(run, RootOptions::default()).await.unwrap()
}

#[tokio::test]
async fn a_reference_held_across_reloads_keeps_only_the_current_generation() {
    let _serial = serial().await;
    let generations = Tracker::default();
    let host = quiet();
    let nothing: Declares = Arc::new(|_, _, _| Ok(()));
    let facet = |id| generation(&generations, id, ServeOptions::default(), nothing.clone());
    host.install(&facet(0), LoadOptions::default()).await.unwrap();
    let reference = host.hold(&store(), json!({}), Default::default()).unwrap();
    reference.ready().await.unwrap();
    for id in 1..=RELOADS {
        reload(&host, facet(id)).await;
        assert_eq!(reference.call_json("get", Json::Null, None).await.unwrap(), json!(id));
    }
    assert_eq!(generations.collect(&[RELOADS]).await, [RELOADS]);
    host.close().await.unwrap();
}

#[tokio::test]
async fn a_host_object_with_state_resource_listener_task_and_registrations_keeps_only_the_current_generation() {
    let _serial = serial().await;
    let generations = Tracker::default();
    let routes = define_registry::<Arc<i64>>("t.gen-routes", None);
    let host = quiet();
    let registered = routes.clone();
    let declare: Declares = Arc::new(move |decl, count, sentinel| {
        // Kept across reloads (same version): adopts each new declaration's close.
        let closing = sentinel.clone();
        let conn = pi_chord::host2::declare::ResourceDeclaration::value(1, ()).with_close(move |_: ()| {
            let _ = &closing;
            std::future::ready(Ok(()))
        });
        decl.resource("conn", conn)?;
        let listening = sentinel.clone();
        decl.on("count", count, move |_, _| {
            let _ = &listening;
            std::future::ready(Ok(()))
        })?;
        let tasked = sentinel.clone();
        decl.task(move |ctx| {
            let tasked = tasked.clone();
            async move {
                ctx.signal().cancelled().await;
                drop(tasked);
                Ok(())
            }
        });
        decl.register(&registered, [("generation".to_string(), sentinel)])
    });
    let facet = |id| generation(&generations, id, ServeOptions::host(), declare.clone());
    host.install(&facet(0), LoadOptions::default()).await.unwrap();
    for id in 1..=RELOADS {
        reload(&host, facet(id)).await;
        assert_eq!(host.registry(&routes).get("generation", None).unwrap().as_deref(), Some(&id));
    }
    assert_eq!(generations.collect(&[RELOADS]).await, [RELOADS]);
    host.close().await.unwrap();
}

#[tokio::test]
async fn abort_listeners_that_calls_and_tasks_add_to_their_signal_do_not_outlive_them() {
    let _serial = serial().await;
    let generations = Tracker::default();
    let host = quiet();
    let declare: Declares = Arc::new(|decl, _, sentinel| {
        decl.task(move |ctx| {
            let tasked = sentinel.clone();
            async move {
                ctx.signal().cancelled().await;
                drop(tasked);
                Ok(())
            }
        });
        Ok(())
    });
    let facet = |id| generation(&generations, id, ServeOptions::host(), declare.clone());
    host.install(&facet(0), LoadOptions::default()).await.unwrap();
    for id in 1..=RELOADS {
        reload(&host, facet(id)).await;
        assert_eq!(get_in_root(&host).await, json!(id));
    }
    assert_eq!(generations.collect(&[RELOADS]).await, [RELOADS]);
    host.close().await.unwrap();
}

#[tokio::test]
async fn disposed_objects_are_not_kept_once_their_disposal_finished() {
    let _serial = serial().await;
    let objects = Tracker::default();
    let item = local_type("t.gen-item", object(&[req("id", number())]), vec![("id", method(None, number()).into())]);
    let host = quiet();
    let tracker = objects.clone();
    let factory = move |input: &Json, _: &Json, _: &mut Declare| {
        // The sentinel stands for the object: its implementation holds it.
        let sentinel = tracker.track(input["id"].as_f64().unwrap_or_default() as i64);
        Ok(Implementation::new().method("id", move |_, _| std::future::ready(Ok(json!(*sentinel)))))
    };
    host.serve(&item, factory, ServeOptions::default()).unwrap();
    for id in 0..50 {
        let reference = host.hold(&item, json!({"id": id}), Default::default()).unwrap();
        assert_eq!(reference.call_json("id", Json::Null, None).await.unwrap(), json!(id));
        reference.release();
    }
    sleep_ms(10).await;
    assert_eq!(objects.collect(&[]).await, Vec::<i64>::new());
    host.close().await.unwrap();
}
