//! Port of upstream `test/services.test.ts`: provider lifecycle, binding rebinds, and keyed observation.

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::delta::parse_ops;
use pi_chord::services::{
    ProviderSubscription, RemoteServiceBinding, RemoteServiceBindingOptions, RemoteServiceProvider,
    RemoteServiceTransport, RemoteServices, Service, ServiceCall, ServiceDefinition, ServiceImpl, ServiceMode,
    ServiceProviderUpdateOf, ServiceSubscription, ServiceSubscriptionSnapshot, ServiceUpdateListener,
    create_loopback_service_transport, create_remote_service_binding, define_service, observe_handler,
    parse_service_subscription_snapshot,
};
use pi_chord::{
    BACKGROUND_CONTEXT, BoxFuture, ChordError, ChordResult, Context, Json, MutableReplicatedState, StateListener, json,
    replicated_state,
};
use support::{Recorder, wait_for};

struct Models;
struct QuestionDialogs;

fn models() -> Service<Models> {
    define_service("test.models").unwrap()
}

fn dialogs() -> Service<QuestionDialogs> {
    define_service("test.question-dialog").unwrap()
}

fn models_state(revision: i64) -> MutableReplicatedState {
    replicated_state(json!({"selected": null, "revision": revision}))
}

fn models_impl(state: &MutableReplicatedState) -> ServiceImpl {
    ServiceImpl::new().state("state", state).method("select", |_, _| async { Ok(None) })
}

fn options(
    services: Vec<pi_chord::services::ServiceRef>,
    provider: &RemoteServiceProvider,
) -> RemoteServiceBindingOptions {
    RemoteServiceBindingOptions::new(services, create_loopback_service_transport(provider))
}

fn revision(value: ChordResult<Option<Json>>) -> Option<f64> {
    value.unwrap().and_then(|value| value["revision"].as_f64())
}

fn errors() -> (Recorder<String>, pi_chord::state::ErrorReporter) {
    let errors = Recorder::new();
    let sink = errors.clone();
    (errors, Arc::new(move |error: ChordError| sink.push(error.to_string())))
}

fn counting(count: &Arc<Mutex<usize>>, fail: bool) -> ServiceUpdateListener {
    let count = count.clone();
    Arc::new(move |_, _| {
        *count.lock().unwrap() += 1;
        if fail { Err(ChordError::error("listener failed")) } else { Ok(()) }
    })
}

#[tokio::test]
async fn keeps_singleton_facades_stable_when_their_provider_is_replaced() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    provider.provide(&models, models_impl(&models_state(1))).unwrap();
    let namespace = create_remote_service_binding(options(vec![models.erase()], &provider)).unwrap();
    let handle = namespace.use_service(&models).unwrap();
    let state = handle.state("state").unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(revision(state.value()), Some(1.0));

    provider.withdraw(&models).unwrap();
    assert_eq!(state.value().unwrap(), None);
    let error = handle.call("select", vec![json!({"provider": "test"})], &BACKGROUND_CONTEXT).await.unwrap_err();
    assert_eq!(error.remote_code().map(|code| code.as_str()), Some("service_not_found"));

    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    let replacement = ServiceImpl::new().state("state", &models_state(2)).method("select", move |_, _| {
        *counter.lock().unwrap() += 1;
        async { Ok(None) }
    });
    provider.replace(&models, replacement).unwrap();
    assert_eq!(revision(state.value()), Some(2.0));
    handle.call("select", vec![json!({"provider": "test"})], &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
    let missing_state = ServiceImpl::new().method("select", |_, _| async { Ok(None) });
    let error = provider.replace(&models, missing_state).unwrap_err();
    assert!(error.to_string().contains("replacement must preserve its member shape"));
    let wrong_kind =
        ServiceImpl::new().method("state", |_, _| async { Ok(None) }).method("select", |_, _| async { Ok(None) });
    assert!(provider.replace(&models, wrong_kind).unwrap_err().to_string().contains("member shape"));
    assert_eq!(revision(state.value()), Some(2.0));
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

#[test]
fn delivers_active_subscriber_updates_before_reporting_listener_failures() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    provider.provide(&models, models_impl(&models_state(1))).unwrap();
    let (failed, delivered) = (Arc::new(Mutex::new(0)), Arc::new(Mutex::new(0)));
    let failing = provider.subscribe("test.models", ServiceMode::Singleton, counting(&failed, true)).unwrap();
    let succeeding = provider.subscribe("test.models", ServiceMode::Singleton, counting(&delivered, false)).unwrap();
    failing.activate().unwrap();
    succeeding.activate().unwrap();
    let error = provider.replace(&models, models_impl(&models_state(2))).unwrap_err();
    assert_eq!(error.to_string(), "listener failed");
    assert_eq!(*delivered.lock().unwrap(), 1);
    failing.close();
    succeeding.close();
    provider.dispose().unwrap();
}

#[test]
fn does_not_replay_a_queued_revision_already_covered_by_a_new_subscription_snapshot() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    let state = models_state(0);
    provider.provide(&models, models_impl(&state)).unwrap();
    let late_updates = Recorder::new();
    let late: Arc<Mutex<Option<ProviderSubscription>>> = Arc::default();
    let (source, slot, sink, inner) = (state.clone(), late.clone(), late_updates.clone(), provider.clone());
    let first = provider
        .subscribe(
            "test.models",
            ServiceMode::Singleton,
            Arc::new(move |update, _| {
                if update.to_json()["sequence"].as_f64() != Some(1.0) {
                    return Ok(());
                }
                source.change(&BACKGROUND_CONTEXT, |draft| draft.set("revision", 2))?;
                let sink = sink.clone();
                let subscription = inner.subscribe(
                    "test.models",
                    ServiceMode::Singleton,
                    Arc::new(move |next, _| {
                        if let Some(sequence) = next.to_json()["sequence"].as_f64() {
                            sink.push(sequence);
                        }
                        Ok(())
                    }),
                )?;
                let members = subscription.snapshot().to_json()["instances"][0]["members"].clone();
                assert!(members.as_array().unwrap().contains(&json!({
                    "name": "state", "kind": "state", "sequence": 2, "ops": [["r", {"selected": null, "revision": 2}]]
                })));
                subscription.activate()?;
                *slot.lock().unwrap() = Some(subscription);
                Ok(())
            }),
        )
        .unwrap();
    first.activate().unwrap();
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("revision", 1)).unwrap();
    assert!(late_updates.get().is_empty());
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("revision", 3)).unwrap();
    assert_eq!(late_updates.get(), [3.0]);
    late.lock().unwrap().take().unwrap().close();
    first.close();
    provider.dispose().unwrap();
}

#[test]
fn replays_every_buffered_update_before_reporting_listener_failures() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    let state = models_state(0);
    provider.provide(&models, models_impl(&state)).unwrap();
    let delivered = Arc::new(Mutex::new(0));
    let subscription = provider.subscribe("test.models", ServiceMode::Singleton, counting(&delivered, true)).unwrap();
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("revision", 1)).unwrap();
    state.change(&BACKGROUND_CONTEXT, |draft| draft.set("revision", 2)).unwrap();
    let error = subscription.activate().unwrap_err();
    assert!(error.to_string().contains("Failed to activate remote service subscription"));
    assert_eq!(*delivered.lock().unwrap(), 2);
    subscription.close();
    provider.dispose().unwrap();
}

#[tokio::test]
async fn clears_retained_facades_when_providers_and_bindings_are_disposed() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    provider.provide(&models, models_impl(&models_state(1))).unwrap();
    let namespace = create_remote_service_binding(options(vec![models.erase()], &provider)).unwrap();
    let handle = namespace.use_service(&models).unwrap();
    let state = handle.state("state").unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(revision(state.value()), Some(1.0));
    provider.dispose().unwrap();
    assert_eq!(state.value().unwrap(), None);
    let error = handle.call("select", vec![], &BACKGROUND_CONTEXT).await.unwrap_err();
    assert!(error.to_string().contains("Remote service provider is disposed"));
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(state.value().unwrap_err().to_string(), "Remote service binding is disposed");
}

#[tokio::test]
async fn applies_provider_disposal_buffered_while_subscriptions_are_starting() {
    let (models, dialogs) = (models(), dialogs());
    let definitions = [ServiceDefinition::singleton(&models), ServiceDefinition::keyed(&dialogs)];
    let provider = RemoteServiceProvider::new(&definitions).unwrap();
    provider.provide(&models, models_impl(&models_state(1))).unwrap();
    let dialog = ServiceImpl::new()
        .state("request", &replicated_state(json!({"question": "Pending?"})))
        .method("submit", |_, _| async { Ok(Some(json!({"accepted": true}))) });
    provider.spawn(&dialogs, "pending", dialog).unwrap();
    let namespace = create_remote_service_binding(options(vec![models.erase(), dialogs.erase()], &provider)).unwrap();
    let handle = namespace.use_service(&models).unwrap();
    let observed = Recorder::new();
    let sink = observed.clone();
    let _stop = namespace.observe(
        &dialogs,
        observe_handler(move |_, _| {
            sink.push(());
            async { Ok(()) }
        }),
    );
    provider.dispose().unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(handle.state("state").unwrap().value().unwrap(), None);
    assert!(observed.get().is_empty());
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
}

#[tokio::test]
async fn keeps_deferred_service_handles_inaccessible_until_host_activation() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    provider.provide(&models, models_impl(&models_state(0))).unwrap();
    let active = Arc::new(Mutex::new(false));
    let flag = active.clone();
    let namespace = create_remote_service_binding(RemoteServiceBindingOptions {
        bound: false,
        assert_access: Some(Arc::new(move || {
            if *flag.lock().unwrap() { Ok(()) } else { Err(ChordError::error("Service handles are not active")) }
        })),
        ..options(vec![models.erase()], &provider)
    })
    .unwrap();
    let handle = namespace.use_service(&models).unwrap();
    let state = handle.state("state").unwrap();
    let inactive = "Service handles are not active";
    assert_eq!(state.value().unwrap_err().to_string(), inactive);
    assert_eq!(state.subscribe(StateListener::sync(|_, _, _| Ok(()))).unwrap_err().to_string(), inactive);
    let call = handle.call("select", vec![], &BACKGROUND_CONTEXT).await;
    assert_eq!(call.unwrap_err().to_string(), inactive);
    namespace.rebind(true, &BACKGROUND_CONTEXT).await.unwrap();
    *active.lock().unwrap() = true;
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(revision(state.value()), Some(0.0));
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

type InvokeFn = Box<dyn Fn(ServiceCall, &Context) -> BoxFuture<ChordResult<Option<Json>>> + Send + Sync>;
type SubscribeFn =
    Box<dyn Fn(&str, ServiceMode, ServiceUpdateListener) -> ChordResult<Arc<dyn ServiceSubscription>> + Send + Sync>;

/// A transport built from closures, standing in for upstream's object literals.
struct FnTransport {
    invoke: InvokeFn,
    subscribe: SubscribeFn,
}

#[async_trait]
impl RemoteServiceTransport for FnTransport {
    async fn invoke(&self, call: ServiceCall, context: &Context) -> ChordResult<Option<Json>> {
        (self.invoke)(call, context).await
    }

    async fn subscribe(
        &self,
        service_id: &str,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        _context: &Context,
    ) -> ChordResult<Arc<dyn ServiceSubscription>> {
        (self.subscribe)(service_id, mode, listener)
    }
}

fn unexpected_invoke() -> InvokeFn {
    Box::new(|_, _| Box::pin(async { Err(ChordError::error("unexpected invocation")) }))
}

struct FixedSubscription(ServiceSubscriptionSnapshot);

impl ServiceSubscription for FixedSubscription {
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot {
        &self.0
    }

    fn activate(&self) -> ChordResult<()> {
        Ok(())
    }

    fn close(&self, _context: &Context) -> BoxFuture<ChordResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn rejects_namespace_readiness_when_initial_hydration_fails() {
    let models = models();
    let transport = FnTransport {
        invoke: unexpected_invoke(),
        subscribe: Box::new(|_, _, _| Err(ChordError::error("initial hydration failed"))),
    };
    let (errors, reporter) = errors();
    let namespace = create_remote_service_binding(RemoteServiceBindingOptions {
        on_error: Some(reporter),
        ..RemoteServiceBindingOptions::new(vec![models.erase()], Arc::new(transport))
    })
    .unwrap();
    let handle = namespace.use_service(&models).unwrap();
    let error = namespace.ready(&BACKGROUND_CONTEXT).await.unwrap_err();
    assert_eq!(error.to_string(), "initial hydration failed");
    assert_eq!(handle.state("state").unwrap().value().unwrap(), None);
    assert_eq!(errors.get(), ["initial hydration failed"]);
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
}

#[tokio::test]
async fn buffers_state_updates_that_race_subscription_hydration() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    let state = models_state(0);
    provider.provide(&models, models_impl(&state)).unwrap();
    let (inner, source) = (provider.clone(), state.clone());
    let transport = FnTransport {
        invoke: unexpected_invoke(),
        subscribe: Box::new(move |id, mode, listener| {
            let subscription = inner.subscribe(id, mode, listener)?;
            source.change(&BACKGROUND_CONTEXT, |draft| draft.set("revision", 1))?;
            Ok(Arc::new(subscription))
        }),
    };
    let namespace =
        create_remote_service_binding(RemoteServiceBindingOptions::new(vec![models.erase()], Arc::new(transport)))
            .unwrap();
    let handle = namespace.use_service(&models).unwrap();
    let revisions = Recorder::new();
    let sink = revisions.clone();
    let _stop = handle.state("state").unwrap().subscribe(StateListener::sync(move |value, _, _| {
        sink.push(value["revision"].as_f64().unwrap());
        Ok(())
    }));
    wait_for(|| revisions.get() == [0.0, 1.0]).await;
    assert_eq!(revision(handle.state("state").unwrap().value()), Some(1.0));
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

async fn clears_state_after_a_bad_sequence(sequence: u64) {
    let models = models();
    let listener: Arc<Mutex<Option<ServiceUpdateListener>>> = Arc::default();
    let slot = listener.clone();
    let snapshot = parse_service_subscription_snapshot(&json!({
        "serviceId": "test.models", "mode": "singleton",
        "instances": [{"members": [
            {"name": "select", "kind": "method"},
            {"name": "state", "kind": "state", "sequence": 0, "ops": [["r", {"selected": null, "revision": 0}]]}
        ]}]
    }))
    .unwrap();
    let transport = FnTransport {
        invoke: unexpected_invoke(),
        subscribe: Box::new(move |_, _, listener| {
            *slot.lock().unwrap() = Some(listener);
            Ok(Arc::new(FixedSubscription(snapshot.clone())))
        }),
    };
    let (errors, reporter) = errors();
    let namespace = create_remote_service_binding(RemoteServiceBindingOptions {
        on_error: Some(reporter),
        ..RemoteServiceBindingOptions::new(vec![models.erase()], Arc::new(transport))
    })
    .unwrap();
    let handle = namespace.use_service(&models).unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(revision(handle.state("state").unwrap().value()), Some(0.0));
    let ops = parse_ops(&json!([["r", {"selected": null, "revision": sequence}]])).unwrap();
    let update = ServiceProviderUpdateOf::State { instance: None, member: "state".into(), sequence, ops };
    let send = listener.lock().unwrap().clone().unwrap();
    send(update, &BACKGROUND_CONTEXT).unwrap();
    assert_eq!(handle.state("state").unwrap().value().unwrap(), None);
    assert_eq!(errors.len(), 1);
    assert!(errors.get()[0].contains("sequence has a gap"));
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
}

#[tokio::test]
async fn clears_replicated_state_after_a_duplicate_operation_sequence() {
    clears_state_after_a_bad_sequence(0).await;
}

#[tokio::test]
async fn clears_replicated_state_after_a_gap_operation_sequence() {
    clears_state_after_a_bad_sequence(2).await;
}

#[tokio::test]
async fn hydrates_cold_replicas_and_replaces_them_across_rebinds() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    let state = models_state(0);
    provider.provide(&models, models_impl(&state)).unwrap();
    let namespace: RemoteServiceBinding = create_remote_service_binding(RemoteServiceBindingOptions {
        bound: false,
        ..options(vec![models.erase()], &provider)
    })
    .unwrap();
    let handle = namespace.use_service(&models).unwrap();
    let revisions = Recorder::new();
    let sink = revisions.clone();
    let replica = handle.state("state").unwrap();
    let _stop = replica.subscribe(StateListener::sync(move |value, _, _| {
        sink.push(value["revision"].as_f64().unwrap());
        Ok(())
    }));
    assert_eq!(replica.value().unwrap(), None);
    let set = |value: i64| state.change(&BACKGROUND_CONTEXT, |draft| draft.set("revision", value)).unwrap();
    set(1);
    namespace.rebind(true, &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(revision(replica.value()), Some(1.0));
    assert_eq!(revisions.get(), [1.0]);
    set(2);
    assert_eq!(revisions.get(), [1.0, 2.0]);
    namespace.rebind(false, &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(replica.value().unwrap(), None);
    set(3);
    assert_eq!(revisions.get(), [1.0, 2.0]);
    namespace.rebind(true, &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(revision(replica.value()), Some(3.0));
    assert_eq!(revisions.get(), [1.0, 2.0, 3.0]);
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

#[test]
fn rejects_mode_mixing() {
    let (models, dialogs) = (models(), dialogs());
    let definitions = [ServiceDefinition::singleton(&models), ServiceDefinition::keyed(&dialogs)];
    let provider = RemoteServiceProvider::new(&definitions).unwrap();
    provider.provide(&models, models_impl(&models_state(0))).unwrap();
    let error = provider.spawn(&models, "wrong", models_impl(&models_state(0))).unwrap_err();
    assert!(error.to_string().contains("singleton"));
    provider.dispose().unwrap();
}
