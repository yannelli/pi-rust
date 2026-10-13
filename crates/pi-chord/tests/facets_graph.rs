//! Port of upstream `test/facets.test.ts`: scoped views, host services, sources, and graph errors.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::FutureExt;
use pi_chord::facets::{Facet, FacetOptions, create_facet_host, define_facet};
use pi_chord::services::{
    ObserveHandler, RemoteServiceBinding, RemoteServiceBindingOptions, RemoteServiceProvider, RemoteServiceSource,
    RemoteServiceSourceOpenOptions, RemoteServices, Service, ServiceCatalogueEntry, ServiceDefinition, ServiceHandle,
    ServiceImpl, ServiceRef, create_loopback_service_transport, create_remote_service_binding, define_local_service,
    define_service,
};
use pi_chord::{
    BACKGROUND_CONTEXT, ChordError, ChordResult, Context, Json, MutableReplicatedState, StateListener, Unsubscribe,
    json,
};
use support::Recorder;

struct Marker;

fn service(id: &str) -> Service<Marker> {
    define_service(id).unwrap()
}

fn reader(value: &str) -> ServiceImpl {
    let value = value.to_string();
    ServiceImpl::new().method("read", move |_, _| {
        let value = value.clone();
        async move { Ok(Some(Json::from(value))) }
    })
}

fn read_now(handle: &ServiceHandle) -> ChordError {
    handle.call("read", vec![], &BACKGROUND_CONTEXT).now_or_never().unwrap().unwrap_err()
}

#[tokio::test]
async fn scopes_singleton_service_views_to_each_facet_lifecycle() {
    let source = service("test.experimental.source");
    let handles: Recorder<ServiceHandle> = Recorder::new();
    let cleanup = Recorder::new();
    let peer_slot: Arc<Mutex<Option<ServiceHandle>>> = Arc::default();
    let consumer = |generation: &str| {
        let (generation, handles, cleanup, source) =
            (generation.to_string(), handles.clone(), cleanup.clone(), source.clone());
        define_facet("scoped-consumer", move |env| {
            let handle = env.use_service(&source)?;
            assert!(env.use_service(&source)?.ptr_eq(&handle));
            handles.push(handle.clone());
            let early = read_now(&handle).to_string();
            assert_eq!(early, "Facet scoped-consumer service handles cannot be used while setting_up");
            let (generation, cleanup) = (generation.clone(), cleanup.clone());
            env.on_deactivate(move || async move {
                let value = handle.call("read", vec![], &BACKGROUND_CONTEXT).await?.unwrap();
                cleanup.push(format!("{generation}:{}", value.as_str().unwrap()));
                Ok(())
            })
        })
    };
    let (peer_source, slot) = (source.clone(), peer_slot.clone());
    let peer = define_facet("peer-consumer", move |env| {
        *slot.lock().unwrap() = Some(env.use_service(&peer_source)?);
        Ok(())
    });
    let provided = source.clone();
    let provider = define_facet("scoped-provider", move |env| env.provide(&provided, reader("value")));
    let host = create_facet_host(FacetOptions::new(vec![consumer("A"), peer, provider])).await.unwrap();
    let peer = peer_slot.lock().unwrap().clone().unwrap();
    assert!(!handles.get()[0].ptr_eq(&peer));

    host.reload(vec![consumer("B")]).await.unwrap();
    assert_eq!(cleanup.get(), ["A:value"]);
    let (old, new) = (handles.get()[0].clone(), handles.get()[1].clone());
    assert!(!old.ptr_eq(&new));
    let dead = "Facet scoped-consumer service handles cannot be used while dead";
    assert_eq!(read_now(&old).to_string(), dead);
    assert_eq!(new.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("value")));
    assert_eq!(peer.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("value")));

    host.dispose().await.unwrap();
    assert_eq!(cleanup.get(), ["A:value", "B:value"]);
    assert_eq!(read_now(&new).to_string(), dead);
    assert_eq!(read_now(&peer).to_string(), "Facet peer-consumer service handles cannot be used while dead");
}

#[tokio::test]
async fn owns_resources_registered_during_activation() {
    let watched = service("test.experimental.watched");
    let state_slot: Arc<Mutex<Option<MutableReplicatedState>>> = Arc::default();
    let deliveries = Arc::new(AtomicUsize::new(0));
    let (token, counter) = (watched.clone(), deliveries.clone());
    let consumer = define_facet("consumer", move |env| {
        let handle = env.use_service(&token)?;
        let (env2, counter) = (env.clone(), counter.clone());
        env.on_activate(move || async move {
            let stop = handle.state("state")?.subscribe(StateListener::sync(move |_, _, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))?;
            env2.own(move || stop.close())
        })
    });
    let (token, slot) = (watched.clone(), state_slot.clone());
    let provider = define_facet("provider", move |env| {
        let state = env.replicated_state(json!({"value": 0}))?;
        *slot.lock().unwrap() = Some(state.clone());
        env.provide(&token, ServiceImpl::new().state("state", &state))
    });
    let host = create_facet_host(FacetOptions::new(vec![consumer, provider])).await.unwrap();
    let state = state_slot.lock().unwrap().clone().unwrap();
    assert_eq!(deliveries.load(Ordering::SeqCst), 1);
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 1)).unwrap();
    assert_eq!(deliveries.load(Ordering::SeqCst), 2);
    host.dispose().await.unwrap();
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 2)).unwrap();
    assert_eq!(deliveries.load(Ordering::SeqCst), 2);
}

struct HostValues {
    name: String,
    use_: String,
}

#[tokio::test]
async fn provides_arbitrary_host_services_through_the_facet_graph() {
    let host_values: Service<HostValues> = define_local_service("test.experimental.host-values").unwrap();
    let token = host_values.clone();
    let consumer = define_facet("host-service-consumer", move |env| {
        let values = env.use_local(&token)?;
        let early = values.get().err().unwrap().to_string();
        assert_eq!(early, "Facet host-service-consumer service handles cannot be used while setting_up");
        env.on_activate(move || async move {
            let values = values.get()?;
            assert_eq!(values.name, "session");
            assert_eq!(values.use_, "host value");
            Ok(())
        })
    });
    let token = host_values.clone();
    let provider = define_facet("host-service-provider", move |env| {
        let values = HostValues { name: "session".into(), use_: "host value".into() };
        env.provide_local(&token, Arc::new(values))
    });
    let host = create_facet_host(FacetOptions::new(vec![consumer, provider])).await.unwrap();
    let error = host.services().use_service(&host_values).unwrap_err().to_string();
    assert_eq!(error, "Service test.experimental.host-values is process-local");
    host.dispose().await.unwrap();
}

/// A binding that subscribes only when the host asks it to get ready, as upstream's sources do.
struct RebindingServices {
    binding: RemoteServiceBinding,
    disposed: Arc<AtomicUsize>,
}

#[async_trait]
impl RemoteServices for RebindingServices {
    fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceHandle> {
        self.binding.use_service(service)
    }

    fn observe(&self, service: &ServiceRef, handler: ObserveHandler) -> ChordResult<Unsubscribe> {
        self.binding.observe(service, handler)
    }

    async fn ready(&self, context: &Context) -> ChordResult<()> {
        self.binding.rebind(true, context).await?;
        self.binding.ready(context).await
    }

    async fn dispose(&self, context: &Context) -> ChordResult<()> {
        self.disposed.fetch_add(1, Ordering::SeqCst);
        self.binding.dispose(context).await
    }
}

struct ProviderSource {
    provider: Mutex<RemoteServiceProvider>,
    opened: AtomicUsize,
    disposed: Arc<AtomicUsize>,
}

impl ProviderSource {
    fn new(provider: &RemoteServiceProvider) -> Arc<Self> {
        Arc::new(Self { provider: Mutex::new(provider.clone()), opened: AtomicUsize::new(0), disposed: Arc::default() })
    }
}

#[async_trait]
impl RemoteServiceSource for ProviderSource {
    fn accepts_unavailable_services(&self) -> bool {
        false
    }

    async fn catalogue(&self, _context: &Context) -> ChordResult<Vec<ServiceCatalogueEntry>> {
        Ok(self.provider.lock().unwrap().catalogue().to_vec())
    }

    fn open(&self, options: RemoteServiceSourceOpenOptions) -> ChordResult<Arc<dyn RemoteServices>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        let provider = self.provider.lock().unwrap().clone();
        let binding = create_remote_service_binding(RemoteServiceBindingOptions {
            bound: false,
            assert_access: Some(options.assert_access),
            on_error: Some(options.on_error),
            ..RemoteServiceBindingOptions::new(options.services, create_loopback_service_transport(&provider))
        })?;
        Ok(Arc::new(RebindingServices { binding, disposed: self.disposed.clone() }))
    }
}

fn single_provider(id: &str, value: &str) -> (Service<Marker>, RemoteServiceProvider) {
    let token = service(id);
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&token)]).unwrap();
    provider.provide(&token, reader(value)).unwrap();
    (token, provider)
}

#[tokio::test]
async fn combines_connected_services_and_facet_provided_services_in_one_host() {
    let (left, left_provider) = single_provider("test.experimental.left-value", "left");
    let (right, right_provider) = single_provider("test.experimental.right-value", "right");
    let combined = service("test.experimental.combined-value");
    let token = combined.clone();
    let facet = define_facet("combined", move |env| {
        let (left, right) = (env.use_service(&left)?, env.use_service(&right)?);
        env.provide(
            &token,
            ServiceImpl::new().method("read", move |_, context| {
                let (left, right) = (left.clone(), right.clone());
                async move {
                    let left = left.call("read", vec![], &context).await?.unwrap();
                    let right = right.call("read", vec![], &context).await?.unwrap();
                    Ok(Some(Json::from(format!("{} {}", left.as_str().unwrap(), right.as_str().unwrap()))))
                }
            }),
        )
    });
    let sources: Vec<Arc<dyn RemoteServiceSource>> =
        vec![ProviderSource::new(&left_provider), ProviderSource::new(&right_provider)];
    let host =
        create_facet_host(FacetOptions { service_sources: sources, ..FacetOptions::new(vec![facet]) }).await.unwrap();
    let published = host.services().use_service(&combined).unwrap();
    assert_eq!(published.call("read", vec![], &BACKGROUND_CONTEXT).await.unwrap(), Some(json!("left right")));
    host.dispose().await.unwrap();
    left_provider.dispose().unwrap();
    right_provider.dispose().unwrap();
}

struct DuplicateSource;

#[async_trait]
impl RemoteServiceSource for DuplicateSource {
    fn accepts_unavailable_services(&self) -> bool {
        false
    }

    async fn catalogue(&self, _context: &Context) -> ChordResult<Vec<ServiceCatalogueEntry>> {
        let entry = pi_chord::services::parse_service_catalogue(&json!([
            {"serviceId": "test.experimental.left-value", "mode": "singleton"}
        ]))?;
        Ok(entry)
    }

    fn open(&self, _options: RemoteServiceSourceOpenOptions) -> ChordResult<Arc<dyn RemoteServices>> {
        Err(ChordError::error("Ambiguous sources must not open"))
    }
}

#[tokio::test]
async fn rejects_a_service_offered_by_multiple_sources() {
    let consumer =
        define_facet("duplicate-consumer", |env| env.use_service(&service("test.experimental.left-value")).map(drop));
    let source: Arc<dyn RemoteServiceSource> = Arc::new(DuplicateSource);
    let options = FacetOptions { service_sources: vec![source.clone(), source], ..FacetOptions::new(vec![consumer]) };
    let error = create_facet_host(options).await.err().unwrap().to_string();
    assert_eq!(error, "Facet host service test.experimental.left-value is offered by more than one source");
}

fn reading_consumer(id: &str, token: &Service<Marker>, values: &Recorder<Json>) -> Arc<dyn Facet> {
    let (token, values) = (token.clone(), values.clone());
    define_facet(id, move |env| {
        let handle = env.use_service(&token)?;
        let values = values.clone();
        env.on_activate(move || async move {
            values.push(handle.call("read", vec![], &BACKGROUND_CONTEXT).await?.unwrap());
            Ok(())
        })
    })
}

#[tokio::test]
async fn reopens_source_bindings_from_changed_catalogues_for_a_replacement_generation() {
    let (left, left_provider) = single_provider("test.experimental.left-value", "left");
    let (right, right_provider) = single_provider("test.experimental.right-value", "right");
    let source = ProviderSource::new(&left_provider);
    let values = Recorder::new();
    let sources: Vec<Arc<dyn RemoteServiceSource>> = vec![source.clone()];
    let first_facets = vec![reading_consumer("left-consumer", &left, &values)];
    let options = FacetOptions { service_sources: sources.clone(), ..FacetOptions::new(first_facets) };
    create_facet_host(options).await.unwrap().dispose().await.unwrap();

    *source.provider.lock().unwrap() = right_provider.clone();
    let second_facets = vec![reading_consumer("right-consumer", &right, &values)];
    let options = FacetOptions { service_sources: sources, ..FacetOptions::new(second_facets) };
    create_facet_host(options).await.unwrap().dispose().await.unwrap();

    assert_eq!(values.get(), [json!("left"), json!("right")]);
    assert_eq!(source.opened.load(Ordering::SeqCst), 2);
    assert_eq!(source.disposed.load(Ordering::SeqCst), 2);
    left_provider.dispose().unwrap();
    right_provider.dispose().unwrap();
}

#[tokio::test]
async fn rejects_missing_dependencies_and_cycles() {
    let activated = Arc::new(AtomicUsize::new(0));
    let flag = activated.clone();
    let missing = define_facet("missing", move |env| {
        env.use_service(&service("test.experimental.source"))?;
        let flag = flag.clone();
        env.on_activate(move || async move {
            flag.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    let error = create_facet_host(FacetOptions::new(vec![missing])).await.err().unwrap().to_string();
    assert_eq!(error, "Facet missing requires local/test.experimental.source/singleton, but no facet provides it");
    assert_eq!(activated.load(Ordering::SeqCst), 0);

    let pair = |id: &str, needs: &str, provides: &str| {
        let (needs, provides) = (service(needs), service(provides));
        define_facet(id, move |env| {
            env.use_service(&needs)?;
            env.provide(&provides, reader("value"))
        })
    };
    let first = pair("first", "test.experimental.projection", "test.experimental.source");
    let second = pair("second", "test.experimental.source", "test.experimental.projection");
    let error = create_facet_host(FacetOptions::new(vec![first, second])).await.err().unwrap().to_string();
    assert_eq!(error, "Facet dependency cycle: first, second");
}
