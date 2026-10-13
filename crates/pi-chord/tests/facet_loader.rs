//! Port of upstream `test/facet-loader.test.ts`: loaders and generation reloads.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::facets::{
    Facet, FacetLoader, FacetOptions, LoadedFacets, combine_facet_loaders, create_facet_host,
    create_static_facet_loader, define_facet,
};
use pi_chord::services::{
    LocalHandle, RemoteServiceBindingOptions, RemoteServices, Service, ServiceHandle, ServiceImpl, ServiceMode,
    create_loopback_service_transport, create_remote_service_binding, define_local_service, define_service,
};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Json, json};
use support::{Gate, Recorder};

trait GenerationValue: Send + Sync {
    fn read(&self) -> String;
}

struct Named(String);

impl GenerationValue for Named {
    fn read(&self) -> String {
        self.0.clone()
    }
}

struct Remote;

fn local_value() -> Service<dyn GenerationValue> {
    define_local_service("test.experimental.local-generation-value").unwrap()
}

fn remote_value() -> Service<Remote> {
    define_service("test.experimental.remote-generation-value").unwrap()
}

fn reader(value: &str) -> ServiceImpl {
    let value = value.to_string();
    ServiceImpl::new().method("read", move |_, _| {
        let value = value.clone();
        async move { Ok(Some(Json::from(value))) }
    })
}

fn noop_facet(id: &str) -> Arc<dyn Facet> {
    define_facet(id, |_| Ok(()))
}

fn traced(trace: &Recorder<String>, entry: &str) -> impl FnOnce() -> std::future::Ready<ChordResult<()>> + use<> {
    let (trace, entry) = (trace.clone(), entry.to_string());
    move || {
        trace.push(entry);
        std::future::ready(Ok(()))
    }
}

struct TracingLoader {
    trace: Recorder<String>,
    facet: Arc<dyn Facet>,
    name: &'static str,
    fail: bool,
}

#[async_trait]
impl FacetLoader for TracingLoader {
    async fn load(&self) -> ChordResult<LoadedFacets> {
        if self.fail {
            return Err(ChordError::error("load failed"));
        }
        self.trace.push(format!("load {}", self.name));
        let (trace, name) = (self.trace.clone(), self.name);
        Ok(LoadedFacets::new(vec![self.facet.clone()], move || async move {
            trace.push(format!("dispose {name}"));
            Ok(())
        }))
    }
}

#[tokio::test]
async fn combines_loaded_facets_in_loader_order_and_disposes_generations_in_reverse() {
    let trace = Recorder::new();
    let (first, second) = (noop_facet("first"), noop_facet("second"));
    let loaders: Vec<Arc<dyn FacetLoader>> = vec![
        Arc::new(TracingLoader { trace: trace.clone(), facet: first.clone(), name: "first", fail: false }),
        Arc::new(TracingLoader { trace: trace.clone(), facet: second.clone(), name: "second", fail: false }),
    ];
    let loaded = combine_facet_loaders(loaders).load().await.unwrap();
    let ids: Vec<&str> = loaded.facets.iter().map(|facet| facet.id()).collect();
    assert_eq!(ids, ["first", "second"]);
    loaded.dispose().await.unwrap();
    loaded.dispose().await.unwrap();
    assert_eq!(trace.get(), ["load first", "load second", "dispose second", "dispose first"]);
}

#[tokio::test]
async fn cleans_up_loaded_facets_when_a_later_loader_fails() {
    let trace = Recorder::new();
    let loaders: Vec<Arc<dyn FacetLoader>> = vec![
        Arc::new(TracingLoader { trace: trace.clone(), facet: noop_facet("first"), name: "first", fail: false }),
        Arc::new(TracingLoader { trace: trace.clone(), facet: noop_facet("second"), name: "second", fail: true }),
    ];
    let error = combine_facet_loaders(loaders).load().await.err().unwrap();
    assert_eq!(error.to_string(), "load failed");
    assert_eq!(trace.get(), ["load first", "dispose first"]);
}

fn generation_provider(trace: &Recorder<String>, name: &'static str, gates: Option<(Gate, Gate)>) -> Arc<dyn Facet> {
    let trace = trace.clone();
    define_facet("provider", move |env| {
        trace.push(format!("setup provider {name}"));
        env.provide_local(&local_value(), Arc::new(Named(name.to_string())) as Arc<dyn GenerationValue>)?;
        env.provide(&remote_value(), reader(name))?;
        let (t, gates) = (trace.clone(), gates.clone());
        env.on_activate(move || async move {
            t.push(format!("activate provider {name}"));
            if let Some((started, proceed)) = gates {
                started.resolve();
                proceed.wait().await?;
            }
            Ok(())
        })?;
        env.on_deactivate(traced(&trace, &format!("deactivate provider {name}")))
    })
}

#[tokio::test]
async fn keeps_local_and_rpc_service_handles_stable_when_their_provider_facet_reloads() {
    let trace = Recorder::new();
    let local_slot: Arc<Mutex<Option<LocalHandle<dyn GenerationValue>>>> = Arc::default();
    let (t, slot) = (trace.clone(), local_slot.clone());
    let consumer = define_facet("consumer", move |env| {
        t.push("setup consumer".to_string());
        let handle = env.use_local(&local_value())?;
        *slot.lock().unwrap() = Some(handle.clone());
        let t2 = t.clone();
        env.on_activate(move || async move {
            t2.push(format!("activate consumer:{}", handle.get()?.read()));
            Ok(())
        })?;
        env.on_deactivate(traced(&t, "deactivate consumer"))
    });
    trace.push("load A".to_string());
    let loaded_a = LoadedFacets::new(vec![generation_provider(&trace, "A", None)], {
        let trace = trace.clone();
        move || async move {
            trace.push("unload A".to_string());
            Ok(())
        }
    });
    let mut facets = vec![consumer];
    facets.extend(loaded_a.facets.clone());
    let host = Arc::new(create_facet_host(FacetOptions::new(facets)).await.unwrap());
    let local = local_slot.lock().unwrap().clone().unwrap();
    let remote = create_remote_service_binding(RemoteServiceBindingOptions::new(
        vec![remote_value().erase()],
        create_loopback_service_transport(host.services()),
    ))
    .unwrap();
    let remote_handle = remote.use_service(&remote_value()).unwrap();
    remote.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let read_remote = |handle: &ServiceHandle| {
        let handle = handle.clone();
        async move { handle.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap().unwrap() }
    };
    assert_eq!(local.get().unwrap().read(), "A");
    assert_eq!(read_remote(&remote_handle).await, json!("A"));

    let invalid = define_facet("provider", |env| {
        env.provide_local(&local_value(), Arc::new(Named("invalid".into())) as Arc<dyn GenerationValue>)
    });
    let error = host.reload(vec![invalid]).await.unwrap_err().to_string();
    assert_eq!(error, "Reloaded facet provider must preserve its service requirements and provisions");
    assert_eq!(local.get().unwrap().read(), "A");
    assert_eq!(read_remote(&remote_handle).await, json!("A"));

    trace.push("load B".to_string());
    let (started, proceed) = (Gate::new(), Gate::new());
    let loaded_b =
        LoadedFacets::new(vec![generation_provider(&trace, "B", Some((started.clone(), proceed.clone())))], {
            let trace = trace.clone();
            move || async move {
                trace.push("unload B".to_string());
                Ok(())
            }
        });
    let (reloading, facets_b) = (host.clone(), loaded_b.facets.clone());
    let reload = tokio::spawn(async move { reloading.reload(facets_b).await });
    started.wait().await.unwrap();
    assert_eq!(local.get().unwrap().read(), "A");
    assert_eq!(read_remote(&remote_handle).await, json!("A"));
    proceed.resolve();
    reload.await.unwrap().unwrap();
    loaded_a.dispose().await.unwrap();

    assert!(remote.use_service(&remote_value()).unwrap().ptr_eq(&remote_handle));
    assert_eq!(local.get().unwrap().read(), "B");
    assert_eq!(read_remote(&remote_handle).await, json!("B"));
    remote.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    host.dispose().await.unwrap();
    loaded_b.dispose().await.unwrap();
    assert_eq!(
        trace.get(),
        [
            "load A",
            "setup consumer",
            "setup provider A",
            "activate provider A",
            "activate consumer:A",
            "load B",
            "setup provider B",
            "activate provider B",
            "deactivate provider A",
            "unload A",
            "deactivate consumer",
            "deactivate provider B",
            "unload B",
        ]
    );
}

#[tokio::test]
async fn rejects_remote_singleton_member_shape_changes_before_reload_cutover() {
    let retained: Arc<Mutex<Option<ServiceHandle>>> = Arc::default();
    let slot = retained.clone();
    let consumer = define_facet("shape-consumer", move |env| {
        *slot.lock().unwrap() = Some(env.use_service(&remote_value())?);
        Ok(())
    });
    let disposed = Arc::new(AtomicBool::new(false));
    let flag = disposed.clone();
    let provider = define_facet("shape-provider", move |env| {
        env.provide(&remote_value(), reader("A"))?;
        let flag = flag.clone();
        env.on_deactivate(move || async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
    });
    let host = create_facet_host(FacetOptions::new(vec![consumer, provider])).await.unwrap();
    let renamed = define_facet("shape-provider", |env| {
        let implementation = ServiceImpl::new().method("renamed", |_, _| async { Ok(Some(json!("B"))) });
        env.provide(&remote_value(), implementation)
    });
    let error = host.reload(vec![renamed]).await.unwrap_err().to_string();
    assert!(error.contains("replacement must preserve its member shape"), "{error}");
    assert!(!disposed.load(Ordering::SeqCst));
    let handle = retained.lock().unwrap().clone().unwrap();
    assert_eq!(handle.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("A")));
    host.dispose().await.unwrap();
    assert!(disposed.load(Ordering::SeqCst));
}

fn simple_provider(id: &str, name: &str, fail_cleanup: bool) -> Arc<dyn Facet> {
    let name = name.to_string();
    define_facet(id, move |env| {
        env.provide(&remote_value(), reader(&name))?;
        env.on_deactivate(move || async move {
            if fail_cleanup { Err(ChordError::error("cleanup failed")) } else { Ok(()) }
        })
    })
}

#[tokio::test]
async fn terminates_the_host_when_old_cleanup_fails_after_cutover() {
    let host =
        create_facet_host(FacetOptions::new(vec![simple_provider("cleanup-provider", "A", true)])).await.unwrap();
    let error = host.reload(vec![simple_provider("cleanup-provider", "B", false)]).await.unwrap_err();
    assert_eq!(error.to_string(), "Facet reload failed after cutover");
    assert_eq!(host.reload(vec![]).await.unwrap_err().to_string(), "Facet host cannot reload while dead");
    host.dispose().await.unwrap();
}

fn ordered(trace: &Recorder<String>, id: &str, label: &str, consumer: bool, fail: bool) -> Arc<dyn Facet> {
    let (trace, label) = (trace.clone(), label.to_string());
    define_facet(id, move |env| {
        if consumer {
            env.use_service(&remote_value())?;
        } else {
            env.provide(&remote_value(), reader(&label))?;
        }
        let (t, entry) = (trace.clone(), format!("activate {label}"));
        env.on_activate(move || async move {
            t.push(entry);
            if fail { Err(ChordError::error("activation failed")) } else { Ok(()) }
        })?;
        env.on_deactivate(traced(&trace, &format!("deactivate {label}")))
    })
}

#[tokio::test]
async fn cleans_failed_candidate_activation_in_reverse_dependency_order() {
    let trace = Recorder::new();
    let facets = vec![
        ordered(&trace, "ordered-consumer", "consumer A", true, false),
        ordered(&trace, "ordered-provider", "provider A", false, false),
    ];
    let host = create_facet_host(FacetOptions::new(facets)).await.unwrap();
    let before = trace.len();
    let error = host
        .reload(vec![
            ordered(&trace, "ordered-consumer", "consumer B", true, true),
            ordered(&trace, "ordered-provider", "provider B", false, false),
        ])
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "activation failed");
    assert_eq!(
        trace.get()[before..],
        ["activate provider B", "activate consumer B", "deactivate consumer B", "deactivate provider B"]
    );
    host.dispose().await.unwrap();
}

#[tokio::test]
async fn terminates_the_host_when_replacement_publication_fails_after_cutover() {
    let host =
        create_facet_host(FacetOptions::new(vec![simple_provider("publication-provider", "A", false)])).await.unwrap();
    let subscription = host
        .services()
        .subscribe(
            "test.experimental.remote-generation-value",
            ServiceMode::Singleton,
            Arc::new(|_, _| Err(ChordError::error("publication failed"))),
        )
        .unwrap();
    subscription.activate().unwrap();
    let error = host.reload(vec![simple_provider("publication-provider", "B", false)]).await.unwrap_err();
    assert_eq!(error.to_string(), "Facet reload failed after cutover");
    assert_eq!(host.reload(vec![]).await.unwrap_err().to_string(), "Facet host cannot reload while dead");
    host.dispose().await.unwrap();
}

#[tokio::test]
async fn keeps_the_old_generation_active_when_replacement_activation_fails_before_cutover() {
    let trace = Recorder::new();
    let retained: Arc<Mutex<Option<ServiceHandle>>> = Arc::default();
    let (slot, t) = (retained.clone(), trace.clone());
    let consumer = define_facet("terminal-consumer", move |env| {
        *slot.lock().unwrap() = Some(env.use_service(&remote_value())?);
        env.on_deactivate(traced(&t, "deactivate consumer"))
    });
    let provider = |name: &str, fail: bool| ordered(&trace, "terminal-provider", name, false, fail);
    let host = create_facet_host(FacetOptions::new(vec![consumer, provider("A", false)])).await.unwrap();
    let handle = retained.lock().unwrap().clone().unwrap();
    assert_eq!(handle.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("A")));

    let error = host.reload(vec![provider("B", true)]).await.unwrap_err();
    assert_eq!(error.to_string(), "activation failed");
    assert_eq!(trace.get(), ["activate A", "activate B", "deactivate B"]);
    assert_eq!(handle.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("A")));
    host.reload(vec![]).await.unwrap();
    host.dispose().await.unwrap();
    assert_eq!(trace.get(), ["activate A", "activate B", "deactivate B", "deactivate consumer", "deactivate A"]);
}

#[tokio::test]
async fn creates_a_reusable_static_loader() {
    let facet = noop_facet("first");
    let loader = create_static_facet_loader(vec![facet.clone()]);
    let (first, second) = (loader.load().await.unwrap(), loader.load().await.unwrap());
    assert!(Arc::ptr_eq(&first.facets[0], &facet));
    assert!(Arc::ptr_eq(&second.facets[0], &facet));
    first.dispose().await.unwrap();
    second.dispose().await.unwrap();
}
