//! Port of upstream `test/facets.test.ts`: activation order, keyed routing, and reloads.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use pi_chord::facets::{Facet, FacetOptions, create_facet_host, define_facet};
use pi_chord::services::{
    RemoteServiceBindingOptions, RemoteServices, Service, ServiceHandle, ServiceImpl, ServiceMode,
    create_loopback_service_transport, create_remote_service_binding, define_local_service, define_service,
    observe_handler,
};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, Context, Json, MutableReplicatedState, StateListener, json};
use support::{Recorder, wait_for};

struct Source;
struct Projection;
struct KeyedValue;
struct Watched;

fn source() -> Service<Source> {
    define_service("test.experimental.source").unwrap()
}

fn projection() -> Service<Projection> {
    define_service("test.experimental.projection").unwrap()
}

fn keyed_value() -> Service<KeyedValue> {
    define_service("test.experimental.keyed-value").unwrap()
}

fn watched() -> Service<Watched> {
    define_service("test.experimental.watched").unwrap()
}

fn reader(value: &str) -> ServiceImpl {
    let value = value.to_string();
    ServiceImpl::new().method("read", move |_, _| {
        let value = value.clone();
        async move { Ok(Some(Json::from(value))) }
    })
}

async fn read(handle: &ServiceHandle, context: &Context) -> Result<Option<Json>, ChordError> {
    handle.call("read", vec![], context).await
}

fn push(trace: &Recorder<String>, entry: &str) -> impl FnOnce() -> std::future::Ready<Result<(), ChordError>> + use<> {
    let (trace, entry) = (trace.clone(), entry.to_string());
    move || {
        trace.push(entry);
        std::future::ready(Ok(()))
    }
}

#[tokio::test]
async fn discovers_setup_dependencies_before_connecting_stable_service_handles() {
    let trace = Recorder::new();
    let handle_slot: Arc<Mutex<Option<ServiceHandle>>> = Arc::default();
    let (t, slot) = (trace.clone(), handle_slot.clone());
    let projection_facet = define_facet("projection", move |env| {
        t.push("setup projection".to_string());
        let handle = env.use_service(&source())?;
        let early = handle.call("read", vec![], &BACKGROUND_CONTEXT).now_or_never().unwrap();
        assert_eq!(early.unwrap_err().to_string(), "Facet projection service handles cannot be used while setting_up");
        *slot.lock().unwrap() = Some(handle.clone());
        env.provide(
            &projection(),
            ServiceImpl::new().method("read", move |_, context| {
                let handle = handle.clone();
                async move { handle.call("read", vec![], &context).await }
            }),
        )?;
        env.on_activate(push(&t, "activate projection"))?;
        env.on_deactivate(push(&t, "dispose projection"))
    });
    let t = trace.clone();
    let source_facet = define_facet("source", move |env| {
        t.push("setup source".to_string());
        env.provide(&source(), reader("value"))?;
        env.on_activate(push(&t, "activate source"))?;
        env.on_deactivate(push(&t, "dispose source"))
    });
    let host = create_facet_host(FacetOptions::new(vec![projection_facet, source_facet])).await.unwrap();
    assert_eq!(trace.get(), ["setup projection", "setup source", "activate source", "activate projection"]);
    let handle = handle_slot.lock().unwrap().clone().unwrap();
    assert_eq!(read(&handle, &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("value")));
    let published = host.services().use_service(&projection()).unwrap();
    assert_eq!(published.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("value")));
    host.dispose().await.unwrap();
    assert_eq!(trace.get()[trace.len() - 2..], ["dispose projection", "dispose source"]);
}

#[tokio::test]
async fn connects_keyed_observations_only_when_the_observing_facet_activates() {
    let trace = Recorder::new();
    let t = trace.clone();
    let observer = define_facet("observer", move |env| {
        let t2 = t.clone();
        env.observe(
            &keyed_value(),
            observe_handler(move |service, context| {
                let t = t2.clone();
                async move {
                    let value = read(&service, &context).await?.unwrap();
                    t.push(format!("observe {}", value.as_str().unwrap()));
                    Ok(())
                }
            }),
        )?;
        env.on_activate(push(&t, "activate observer"))
    });
    let t = trace.clone();
    let provider = define_facet("provider", move |env| {
        let values = env.provide_many(&keyed_value())?;
        let t = t.clone();
        env.on_activate(move || async move {
            t.push("activate provider".to_string());
            values.spawn("one", reader("one")).map(drop)
        })
    });
    let host = create_facet_host(FacetOptions::new(vec![observer, provider])).await.unwrap();
    wait_for(|| trace.get().contains(&"observe one".to_string())).await;
    assert_eq!(trace.get(), ["activate provider", "activate observer", "observe one"]);

    let remote = create_remote_service_binding(RemoteServiceBindingOptions::new(
        vec![keyed_value().erase()],
        create_loopback_service_transport(host.services()),
    ))
    .unwrap();
    let remote_values = Recorder::new();
    let sink = remote_values.clone();
    let _stop = remote
        .observe(
            &keyed_value(),
            observe_handler(move |service, context| {
                let sink = sink.clone();
                async move {
                    sink.push(read(&service, &context).await?.unwrap());
                    Ok(())
                }
            }),
        )
        .unwrap();
    remote.ready(&BACKGROUND_CONTEXT).await.unwrap();
    wait_for(|| remote_values.get() == [json!("one")]).await;
    remote.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    host.dispose().await.unwrap();
}

fn keyed_provider(id: &str, value: &str) -> Arc<dyn Facet> {
    let value = value.to_string();
    define_facet(id, move |env| {
        let values = env.provide_many(&keyed_value())?;
        let value = value.clone();
        env.on_activate(move || async move { values.spawn("current", reader(&value)).map(drop) })
    })
}

type Observed = Recorder<(ServiceHandle, Context)>;

fn keyed_consumer(observed: &Observed) -> Arc<dyn Facet> {
    let observed = observed.clone();
    define_facet("remote-keyed-consumer", move |env| {
        let sink = observed.clone();
        env.observe(
            &keyed_value(),
            observe_handler(move |service, context| {
                sink.push((service, context));
                async { Ok(()) }
            }),
        )
    })
}

#[tokio::test]
async fn routes_remotely_exposable_keyed_services_through_the_host_provider() {
    let observed: Observed = Recorder::new();
    let facets = vec![keyed_consumer(&observed), keyed_provider("remote-keyed-provider", "A")];
    let host = create_facet_host(FacetOptions::new(facets)).await.unwrap();
    wait_for(|| observed.len() == 1).await;
    let (first, first_context) = observed.get()[0].clone();
    assert_eq!(read(&first, &first_context).await.unwrap(), Some(json!("A")));

    host.reload(vec![keyed_provider("remote-keyed-provider", "B")]).await.unwrap();
    wait_for(|| observed.len() == 2).await;
    assert!(first_context.is_cancelled());
    let closed = read(&first, &first_context).await.unwrap_err();
    assert!(closed.to_string().contains("observation is closed"));
    let (second, second_context) = observed.get()[1].clone();
    assert_eq!(read(&second, &second_context).await.unwrap(), Some(json!("B")));
    host.dispose().await.unwrap();
    assert!(second_context.is_cancelled());
}

async fn terminates_when_publication_fails(failing_type: &'static str) {
    let id = "failing-keyed-provider";
    let host = create_facet_host(FacetOptions::new(vec![keyed_provider(id, "A")])).await.unwrap();
    let subscription = host
        .services()
        .subscribe(
            "test.experimental.keyed-value",
            ServiceMode::Keyed,
            Arc::new(move |update, _| {
                if update.type_name() == failing_type {
                    return Err(ChordError::error("publication failed"));
                }
                Ok(())
            }),
        )
        .unwrap();
    subscription.activate().unwrap();
    let error = host.reload(vec![keyed_provider(id, "B")]).await.unwrap_err();
    assert_eq!(error.to_string(), "Facet reload failed after cutover");
    let dead = host.reload(vec![]).await.unwrap_err();
    assert_eq!(dead.to_string(), "Facet host cannot reload while dead");
    let used = host.services().use_service(&keyed_value()).unwrap_err();
    assert_eq!(used.to_string(), "Remote service provider is disposed");
    host.dispose().await.unwrap();
}

#[tokio::test]
async fn terminates_the_host_when_keyed_replacement_publication_fails() {
    terminates_when_publication_fails("spawned").await;
}

#[tokio::test]
async fn terminates_the_host_when_keyed_retirement_publication_fails() {
    terminates_when_publication_fails("closed").await;
}

struct LocalKeyedValue {
    metadata: HashMap<String, String>,
    value: String,
}

fn local_keyed_value() -> Service<LocalKeyedValue> {
    define_local_service("test.experimental.local-keyed-value").unwrap()
}

#[tokio::test]
async fn keeps_unrestricted_local_keyed_services_process_local_across_provider_reloads() {
    let observed: Recorder<(pi_chord::services::LocalHandle<LocalKeyedValue>, Context)> = Recorder::new();
    let sink = observed.clone();
    let consumer = define_facet("local-keyed-consumer", move |env| {
        let sink = sink.clone();
        env.observe_local(&local_keyed_value(), move |service, context| {
            sink.push((service, context));
            async { Ok(()) }
        })
    });
    let provider = |value: &str| {
        let value = value.to_string();
        define_facet("local-keyed-provider", move |env| {
            let values = env.provide_many_local(&local_keyed_value())?;
            let value = value.clone();
            env.on_activate(move || async move {
                let metadata = HashMap::from([("value".to_string(), value.clone())]);
                values.spawn("current", Arc::new(LocalKeyedValue { metadata, value })).map(drop)
            })
        })
    };
    let host = create_facet_host(FacetOptions::new(vec![consumer, provider("A")])).await.unwrap();
    wait_for(|| observed.len() == 1).await;
    let (first, first_context) = observed.get()[0].clone();
    assert_eq!(first.get().unwrap().value, "A");
    assert_eq!(first.get().unwrap().metadata["value"], "A");
    assert!(host.services().catalogue().iter().all(|entry| entry.service_id != "test.experimental.local-keyed-value"));
    assert!(host.services().use_service(&local_keyed_value()).unwrap_err().to_string().contains("process-local"));

    host.reload(vec![provider("B")]).await.unwrap();
    wait_for(|| observed.len() == 2).await;
    assert!(first_context.is_cancelled());
    let closed = first.get().err().unwrap().to_string();
    assert_eq!(closed, "Keyed service test.experimental.local-keyed-value observation is closed");
    let (second, second_context) = observed.get()[1].clone();
    assert_eq!(second.get().unwrap().value, "B");
    assert_eq!(second.get().unwrap().metadata["value"], "B");
    host.dispose().await.unwrap();
    assert!(second_context.is_cancelled());
}

#[tokio::test]
async fn keeps_remotely_exposable_local_state_replicas_stable_across_provider_reloads() {
    let sources: Recorder<MutableReplicatedState> = Recorder::new();
    let revisions = Recorder::new();
    let watched_slot: Arc<Mutex<Option<ServiceHandle>>> = Arc::default();
    let (slot, sink) = (watched_slot.clone(), revisions.clone());
    let consumer = define_facet("state-consumer", move |env| {
        let handle = env.use_service(&watched())?;
        *slot.lock().unwrap() = Some(handle.clone());
        let (env2, sink) = (env.clone(), sink.clone());
        env.on_activate(move || async move {
            let stop = handle.state("state")?.subscribe(StateListener::sync(move |value, _, _| {
                sink.push(value["value"].clone());
                Ok(())
            }))?;
            env2.own(move || stop.close())
        })
    });
    let provider = |value: i64| {
        let sources = sources.clone();
        define_facet("state-provider", move |env| {
            let state = env.replicated_state(json!({"value": value}))?;
            sources.push(state.clone());
            env.provide(&watched(), ServiceImpl::new().state("state", &state))
        })
    };
    let host = create_facet_host(FacetOptions::new(vec![consumer, provider(1)])).await.unwrap();
    let retained = watched_slot.lock().unwrap().clone().unwrap().state("state").unwrap();
    assert_eq!(retained.value().unwrap(), Some(json!({"value": 1})));
    assert_eq!(revisions.get(), [json!(1)]);

    host.reload(vec![provider(2)]).await.unwrap();
    assert_eq!(retained.value().unwrap(), Some(json!({"value": 2})));
    assert_eq!(revisions.get(), [json!(1), json!(2)]);
    sources.get()[0].change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 3)).unwrap();
    assert_eq!(retained.value().unwrap(), Some(json!({"value": 2})));
    assert_eq!(revisions.get(), [json!(1), json!(2)]);
    host.dispose().await.unwrap();
    let dead = retained.value().unwrap_err().to_string();
    assert_eq!(dead, "Facet state-consumer service handles cannot be used while dead");
}
