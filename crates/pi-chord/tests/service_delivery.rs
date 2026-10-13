//! Port of upstream `test/service-delivery.test.ts`: provider delivery queues and wire rebaselines.

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::services::{
    RemoteServiceBindingOptions, RemoteServiceProvider, RemoteServiceTransport, RemoteServices, Service, ServiceCall,
    ServiceDefinition, ServiceImpl, ServiceMode, ServiceProviderUpdate, ServiceSubscription,
    ServiceSubscriptionSnapshot, ServiceUpdateListener, create_remote_service_binding, create_service_state_decoder,
    create_service_state_encoder, define_service, observe_handler, parse_wire_service_provider_update,
    parse_wire_service_subscription_snapshot,
};
use pi_chord::{
    BACKGROUND_CONTEXT, BoxFuture, ChordError, ChordResult, Context, Json, MutableReplicatedState, json,
    replicated_state, with_cancel,
};
use support::{Recorder, matches_object};

struct Counter;

fn counter() -> Service<Counter> {
    define_service("test.delivery-counter").unwrap()
}

fn counter_impl(state: &MutableReplicatedState) -> ServiceImpl {
    ServiceImpl::new().state("state", state)
}

fn singleton_provider(state: &MutableReplicatedState) -> RemoteServiceProvider {
    let counter = counter();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&counter)]).unwrap();
    provider.provide(&counter, counter_impl(state)).unwrap();
    provider
}

fn set(state: &MutableReplicatedState, value: i64) {
    state.replace(&BACKGROUND_CONTEXT, json!({"value": value})).unwrap();
}

fn sequence(update: &Json) -> Option<f64> {
    update["sequence"].as_f64()
}

fn recorder_listener(recorder: &Recorder<Json>) -> ServiceUpdateListener {
    let recorder = recorder.clone();
    Arc::new(move |update: ServiceProviderUpdate, _| {
        recorder.push(update.to_json());
        Ok(())
    })
}

#[test]
fn appends_reentrant_updates_to_the_activation_fifo() {
    let state = replicated_state(json!({"value": 0}));
    let provider = singleton_provider(&state);
    let received = Recorder::new();
    let (sink, source) = (received.clone(), state.clone());
    let subscription = provider
        .subscribe(
            "test.delivery-counter",
            ServiceMode::Singleton,
            Arc::new(move |update, _| {
                let Some(sequence) = sequence(&update.to_json()) else { return Ok(()) };
                sink.push(sequence);
                if sequence == 1.0 {
                    set(&source, 3);
                }
                Ok(())
            }),
        )
        .unwrap();
    set(&state, 1);
    set(&state, 2);
    subscription.activate().unwrap();
    subscription.activate().unwrap();
    assert_eq!(received.get(), [1.0, 2.0, 3.0]);
    provider.dispose().unwrap();
}

#[test]
fn bounds_pending_updates_with_explicit_root_resets() {
    for count in [100_i64, 101, 102, 201, 202] {
        let state = replicated_state(json!({"value": 0}));
        let provider = singleton_provider(&state);
        let updates = Recorder::new();
        let contexts: Recorder<Context> = Recorder::new();
        let (sink, context_sink) = (updates.clone(), contexts.clone());
        let subscription = provider
            .subscribe(
                "test.delivery-counter",
                ServiceMode::Singleton,
                Arc::new(move |update, context| {
                    sink.push(update.to_json());
                    context_sink.push(context.clone());
                    Ok(())
                }),
            )
            .unwrap();
        let (context, _controller) = with_cancel(&BACKGROUND_CONTEXT);
        for value in 1..=count {
            state.replace(&context, json!({"value": value})).unwrap();
        }
        assert!(updates.get().is_empty());
        assert_eq!(subscription.snapshot().to_json()["instances"][0]["members"][0]["sequence"], json!(0));
        subscription.activate().unwrap();
        let first = (count - 1) / 100 * 100 + 1;
        assert_eq!(updates.len() as i64, count - first + 1, "count {count}");
        if count > 100 {
            assert_eq!(
                updates.get()[0],
                json!({"type": "reset", "snapshot": {"serviceId": "test.delivery-counter", "mode": "singleton",
                    "instances": [{"members": [{"name": "state", "kind": "state", "sequence": first,
                        "ops": [["r", {"value": first}]]}]}]}})
            );
        }
        assert!(contexts.get().iter().all(|received| received.ptr_eq(&context)));
        state.replace(&context, json!({"value": count + 1})).unwrap();
        let last = updates.get().last().cloned().unwrap();
        assert!(matches_object(&last, &json!({"type": "state", "sequence": count + 1})));
        provider.dispose().unwrap();
    }
}

#[test]
fn does_not_compact_the_running_delivery_when_activation_overflows_reentrantly() {
    let state = replicated_state(json!({"value": 0}));
    let provider = singleton_provider(&state);
    let updates = Recorder::new();
    let (sink, source) = (updates.clone(), state.clone());
    let subscription = provider
        .subscribe(
            "test.delivery-counter",
            ServiceMode::Singleton,
            Arc::new(move |update, _| {
                let update = update.to_json();
                sink.push(update.clone());
                if update["type"] == json!("state") && sequence(&update) == Some(1.0) {
                    for value in 3..=103 {
                        set(&source, value);
                    }
                }
                Ok(())
            }),
        )
        .unwrap();
    set(&state, 1);
    set(&state, 2);
    subscription.activate().unwrap();
    let updates = updates.get();
    assert_eq!(updates.len(), 3);
    assert!(matches_object(&updates[0], &json!({"type": "state", "sequence": 1})));
    let reset = json!({"type": "reset",
        "snapshot": {"instances": [{"members": [{"sequence": 102, "ops": [["r", {"value": 102}]]}]}]}});
    assert!(matches_object(&updates[1], &reset));
    assert!(matches_object(&updates[2], &json!({"type": "state", "sequence": 103})));
    provider.dispose().unwrap();
}

#[test]
fn suppresses_notifications_already_covered_by_an_overflow_snapshot() {
    let state = replicated_state(json!({"value": 0}));
    let source = state.clone();
    let _ops = state.member().subscribe_ops(move |_, sequence, _| {
        if sequence == 101 {
            set(&source, 102);
            set(&source, 103);
        }
        Ok(())
    });
    let provider = singleton_provider(&state);
    let updates = Recorder::new();
    let subscription =
        provider.subscribe("test.delivery-counter", ServiceMode::Singleton, recorder_listener(&updates)).unwrap();
    for value in 1..=101 {
        set(&state, value);
    }
    subscription.activate().unwrap();
    assert_eq!(updates.len(), 1);
    let reset = json!({"type": "reset",
        "snapshot": {"instances": [{"members": [{"sequence": 103, "ops": [["r", {"value": 103}]]}]}]}});
    assert!(matches_object(&updates.get()[0], &reset));
    set(&state, 104);
    assert!(matches_object(&updates.get()[1], &json!({"type": "state", "sequence": 104})));
    provider.dispose().unwrap();
}

#[test]
fn preserves_lifecycle_ordering_across_subscribers_during_reentrant_publication() {
    let counter = counter();
    let provider = singleton_provider(&replicated_state(json!({"value": 0})));
    let (first, second) = (Recorder::new(), Recorder::new());
    let (sink, inner, token) = (first.clone(), provider.clone(), counter.clone());
    let a = provider
        .subscribe(
            "test.delivery-counter",
            ServiceMode::Singleton,
            Arc::new(move |update, _| {
                sink.push(update.to_json());
                if sink.len() == 1 {
                    inner.replace(&token, counter_impl(&replicated_state(json!({"value": 2}))))?;
                }
                Ok(())
            }),
        )
        .unwrap();
    a.activate().unwrap();
    let b = provider.subscribe("test.delivery-counter", ServiceMode::Singleton, recorder_listener(&second)).unwrap();
    b.activate().unwrap();
    provider.replace(&counter, counter_impl(&replicated_state(json!({"value": 1})))).unwrap();
    assert_eq!(first.get(), second.get());
    assert_eq!(second.len(), 2);
    let replaced = json!({"type": "replaced", "snapshot": {"members": [{"ops": [["r", {"value": 1}]]}]}});
    assert!(matches_object(&second.get()[0], &replaced));
    provider.dispose().unwrap();
}

#[test]
fn close_during_a_callback_prevents_later_buffered_callbacks() {
    let state = replicated_state(json!({"value": 0}));
    let provider = singleton_provider(&state);
    let received = Recorder::new();
    let slot: Arc<std::sync::OnceLock<Arc<pi_chord::services::ProviderSubscription>>> = Arc::default();
    let (sink, handle) = (received.clone(), slot.clone());
    let subscription = provider
        .subscribe(
            "test.delivery-counter",
            ServiceMode::Singleton,
            Arc::new(move |update, _| {
                sink.push(update.to_json());
                if let Some(subscription) = handle.get() {
                    subscription.close();
                }
                Ok(())
            }),
        )
        .unwrap();
    set(&state, 1);
    set(&state, 2);
    let subscription = Arc::new(subscription);
    let _ = slot.set(subscription.clone());
    subscription.activate().unwrap();
    assert_eq!(received.len(), 1);
    provider.dispose().unwrap();
}

#[test]
fn drains_terminal_updates_when_disposed_reentrantly() {
    let state = replicated_state(json!({"value": 0}));
    let provider = singleton_provider(&state);
    let received = Recorder::new();
    let (sink, inner) = (received.clone(), provider.clone());
    let subscription = provider
        .subscribe(
            "test.delivery-counter",
            ServiceMode::Singleton,
            Arc::new(move |update, _| {
                sink.push(update.type_name().to_string());
                if update.type_name() == "state" {
                    inner.dispose()?;
                }
                Ok(())
            }),
        )
        .unwrap();
    set(&state, 1);
    subscription.activate().unwrap();
    assert_eq!(received.get(), ["state", "unavailable"]);
}

/// A transport that round-trips every snapshot and update through the wire codecs and JSON parsers.
struct WireTransport {
    provider: RemoteServiceProvider,
    before_activate: Arc<dyn Fn() + Send + Sync>,
    updates: Recorder<Json>,
}

struct WireSubscription {
    snapshot: ServiceSubscriptionSnapshot,
    inner: pi_chord::services::ProviderSubscription,
    before_activate: Arc<dyn Fn() + Send + Sync>,
}

impl ServiceSubscription for WireSubscription {
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot {
        &self.snapshot
    }

    fn activate(&self) -> ChordResult<()> {
        (self.before_activate)();
        self.inner.activate()
    }

    fn close(&self, _context: &Context) -> BoxFuture<ChordResult<()>> {
        self.inner.close();
        Box::pin(async { Ok(()) })
    }
}

#[async_trait]
impl RemoteServiceTransport for WireTransport {
    async fn invoke(&self, call: ServiceCall, context: &Context) -> ChordResult<Option<Json>> {
        self.provider.invoke(call, context).await
    }

    async fn subscribe(
        &self,
        service_id: &str,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        _context: &Context,
    ) -> ChordResult<Arc<dyn ServiceSubscription>> {
        let encoder = Arc::new(Mutex::new(create_service_state_encoder()));
        let decoder = Arc::new(Mutex::new(create_service_state_decoder()));
        let (enc, dec, updates) = (encoder.clone(), decoder.clone(), self.updates.clone());
        let inner = self.provider.subscribe(
            service_id,
            mode,
            Arc::new(move |update, context| {
                let wire = enc.lock().unwrap().encode_update(&update)?;
                let parsed = parse_wire_service_provider_update(&wire.to_json())?;
                let decoded = dec.lock().unwrap().decode_update(&parsed)?;
                updates.push(decoded.to_json());
                listener(decoded, context)
            }),
        )?;
        let wire = encoder.lock().unwrap().encode_snapshot(inner.snapshot())?;
        let parsed = parse_wire_service_subscription_snapshot(&wire.to_json())?;
        let snapshot = decoder.lock().unwrap().decode_snapshot(&parsed)?;
        Ok(Arc::new(WireSubscription { snapshot, inner, before_activate: self.before_activate.clone() }))
    }
}

fn errors() -> (Recorder<String>, pi_chord::state::ErrorReporter) {
    let errors = Recorder::new();
    let sink = errors.clone();
    (errors, Arc::new(move |error: ChordError| sink.push(error.to_string())))
}

fn wire_binding(
    provider: &RemoteServiceProvider,
    service: &pi_chord::services::ServiceRef,
    before_activate: impl Fn() + Send + Sync + 'static,
) -> (pi_chord::services::RemoteServiceBinding, Recorder<Json>, Recorder<String>) {
    let updates = Recorder::new();
    let transport = WireTransport {
        provider: provider.clone(),
        before_activate: Arc::new(before_activate),
        updates: updates.clone(),
    };
    let (errors, reporter) = errors();
    let binding = create_remote_service_binding(RemoteServiceBindingOptions {
        on_error: Some(reporter),
        ..RemoteServiceBindingOptions::new(vec![service.clone()], Arc::new(transport))
    })
    .unwrap();
    (binding, updates, errors)
}

#[tokio::test]
async fn rebaselines_every_member_through_wire_codecs_then_resumes_contiguous_deltas() {
    let pair = define_service::<()>("test.delivery-pair").unwrap();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&pair)]).unwrap();
    let (left, right) = (replicated_state(json!({"value": 0})), replicated_state(json!({"value": 0})));
    provider.provide(&pair, ServiceImpl::new().state("left", &left).state("right", &right)).unwrap();
    let both = move |left: &MutableReplicatedState, right: &MutableReplicatedState, value: i64| {
        left.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", value)).unwrap();
        right.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", value)).unwrap();
    };
    let (l, r) = (left.clone(), right.clone());
    let (namespace, updates, errors) = wire_binding(&provider, &pair, move || {
        for value in 1..=101 {
            both(&l, &r, value);
        }
    });
    let handle = namespace.use_service(&pair).unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let types: Vec<Json> = updates.get().iter().map(|update| update["type"].clone()).collect();
    assert_eq!(types, [json!("reset"), json!("state")]);
    assert_eq!(handle.state("left").unwrap().value().unwrap(), Some(json!({"value": 101})));
    assert_eq!(handle.state("right").unwrap().value().unwrap(), Some(json!({"value": 101})));
    for value in 102..=104 {
        both(&left, &right, value);
    }
    assert_eq!(handle.state("left").unwrap().value().unwrap(), Some(json!({"value": 104})));
    assert_eq!(handle.state("right").unwrap().value().unwrap(), Some(json!({"value": 104})));
    assert!(errors.get().is_empty());
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

#[tokio::test]
async fn an_overflow_reset_can_make_a_singleton_unavailable_before_a_later_replacement() {
    let counter = counter();
    let provider = singleton_provider(&replicated_state(json!({"value": 0})));
    let (inner, token) = (provider.clone(), counter.clone());
    let (namespace, updates, errors) = wire_binding(&provider, &counter, move || {
        for value in 1..=100 {
            inner.replace(&token, counter_impl(&replicated_state(json!({"value": value})))).unwrap();
        }
        inner.withdraw(&token).unwrap();
    });
    let handle = namespace.use_service(&counter).unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(
        updates.get(),
        [json!({"type": "reset",
        "snapshot": {"serviceId": "test.delivery-counter", "mode": "singleton", "instances": []}})]
    );
    let state = handle.state("state").unwrap();
    assert_eq!(state.value().unwrap(), None);
    let replacement = replicated_state(json!({"value": 200}));
    provider.replace(&counter, counter_impl(&replacement)).unwrap();
    replacement.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 201)).unwrap();
    assert_eq!(state.value().unwrap(), Some(json!({"value": 201})));
    assert!(errors.get().is_empty());
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

#[tokio::test]
async fn keyed_resets_keep_live_generations_and_reconcile_closed_and_reused_keys() {
    let counter = counter();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::keyed(&counter)]).unwrap();
    let kept = replicated_state(json!({"value": 0}));
    provider.spawn(&counter, "retained", counter_impl(&kept)).unwrap();
    let close_old = provider.spawn(&counter, "reused", counter_impl(&replicated_state(json!({"value": 0})))).unwrap();
    let removed = provider.spawn(&counter, "removed", counter_impl(&replicated_state(json!({"value": 0})))).unwrap();
    let (namespace, updates, errors) = wire_binding(&provider, &counter, || {});
    let observed: Recorder<(pi_chord::services::ServiceHandle, Context)> = Recorder::new();
    let sink = observed.clone();
    let _stop = namespace
        .observe(
            &counter,
            observe_handler(move |service, context| {
                sink.push((service, context));
                async { Ok(()) }
            }),
        )
        .unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(observed.len(), 3);
    // Provider snapshots are sorted by key.
    let (stable, old) = (observed.get()[1].clone(), observed.get()[2].clone());
    let replacement = replicated_state(json!({"value": 500}));
    let (inner, token, next_state, kept_state) = (provider.clone(), counter.clone(), replacement.clone(), kept.clone());
    let _watch = stable.0.state("state").unwrap().subscribe(pi_chord::StateListener::sync(move |value, _, _| {
        if value["value"].as_f64() != Some(1.0) {
            return Ok(());
        }
        close_old.unsubscribe();
        removed.unsubscribe();
        inner.spawn(&token, "reused", counter_impl(&next_state))?;
        set(&kept_state, 102);
        for next in 501..=601 {
            set(&next_state, next);
        }
        Ok(())
    }));
    set(&kept, 1);
    assert!(updates.get().iter().any(|update| update["type"] == json!("reset")));
    assert_eq!(observed.len(), 4);
    let observed = observed.get();
    assert!(!stable.1.is_cancelled());
    assert_eq!(stable.0.state("state").unwrap().value().unwrap(), Some(json!({"value": 102})));
    assert!(old.1.is_cancelled());
    assert!(observed[0].1.is_cancelled());
    let fourth = observed[3].0.state("state").unwrap();
    assert_eq!(fourth.value().unwrap(), Some(json!({"value": 601})));
    replacement.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 602)).unwrap();
    assert_eq!(fourth.value().unwrap(), Some(json!({"value": 602})));
    assert!(errors.get().is_empty(), "{:?}", errors.get());
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}
