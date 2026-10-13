//! Port of upstream `test/services.test.ts`: tokens, providers, and singleton bindings.

mod support;

use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;
use pi_chord::services::{
    RemoteServiceBinding, RemoteServiceBindingOptions, RemoteServiceProvider, RemoteServices, Service,
    ServiceDefinition, ServiceImpl, ServiceMemberSnapshotOf, ServiceMode, ServiceProviderUpdate, ServiceRef,
    create_loopback_service_transport, create_remote_service_binding, define_local_service, define_service,
};
use pi_chord::state::ErrorReporter;
use pi_chord::{
    BACKGROUND_CONTEXT, ChordError, Json, ReplicatedStateSource, ReplicatedStateSourceAttachment,
    ReplicatedStateSourceFrame, ReplicatedStateSourceOptions, ReplicatedStateSourceSnapshot, StateListener,
    attach_replicated_state_source, json, replicated_state,
};
use support::Recorder;

struct Models;
struct Echo;
struct Timeline;

fn models() -> Service<Models> {
    define_service("test.models").unwrap()
}

fn binding(services: &[&ServiceRef], provider: &RemoteServiceProvider) -> RemoteServiceBinding {
    let services = services.iter().map(|service| (*service).clone()).collect();
    create_remote_service_binding(RemoteServiceBindingOptions::new(
        services,
        create_loopback_service_transport(provider),
    ))
    .unwrap()
}

fn error_recorder() -> (Recorder<String>, ErrorReporter) {
    let errors = Recorder::new();
    let sink = errors.clone();
    (errors, Arc::new(move |error: ChordError| sink.push(error.to_string())))
}

fn updates_listener(recorder: &Recorder<Json>) -> pi_chord::services::ServiceUpdateListener {
    let recorder = recorder.clone();
    Arc::new(move |update: ServiceProviderUpdate, _| {
        recorder.push(update.to_json());
        Ok(())
    })
}

fn revision(value: Option<Json>) -> Option<f64> {
    value.and_then(|value| value.get("revision").and_then(Json::as_f64))
}

#[test]
fn marks_services_remotable_by_default_and_reserves_chord_ids() {
    let local = define_local_service::<()>("test.local").unwrap();
    assert!(!models().is_local());
    assert!(local.is_local());
    let reserved = define_local_service::<()>("$chord.internal").unwrap_err();
    assert_eq!(reserved.to_string(), "Service IDs beginning with $chord. are reserved");
    let error = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&local)]).unwrap_err();
    assert!(error.to_string().contains("cannot be published remotely"));
}

#[test]
fn tracks_mutable_source_state_while_publishing_immutable_revisions() {
    let root = json!({"selected": null, "revision": 0});
    let state = replicated_state(root.clone());
    let delivered: Arc<Mutex<Option<Json>>> = Arc::default();
    let kinds = Recorder::new();
    let (sink, kind_sink) = (delivered.clone(), kinds.clone());
    let stop = state.subscribe(StateListener::sync(move |value, _, delivery| {
        *sink.lock().unwrap() = Some(value.clone());
        kind_sink.push(format!("{:?}", delivery.kind));
        Ok(())
    }));
    assert!(state.value().same_ref(&root));
    let hydrated = delivered.lock().unwrap().clone().unwrap();
    state
        .change(&BACKGROUND_CONTEXT, |draft| {
            draft.set("selected", json!({"provider": "test", "modelId": "one"}))?;
            draft.set("revision", 1)
        })
        .unwrap();
    assert_eq!(state.value(), json!({"selected": {"provider": "test", "modelId": "one"}, "revision": 1}));
    assert!(!state.value().same_ref(&root));
    assert!(delivered.lock().unwrap().as_ref().unwrap().same_ref(&state.value()));
    assert_eq!(hydrated, json!({"selected": null, "revision": 0}));
    assert_eq!(kinds.get(), ["Hydrate", "Update"]);
    stop.unsubscribe();
}

#[test]
fn does_not_publish_when_a_transaction_restores_the_prior_value() {
    let state = replicated_state(json!({"value": 1}));
    let deliveries = Recorder::new();
    let sink = deliveries.clone();
    let _stop = state.subscribe(StateListener::sync(move |_, _, delivery| {
        sink.push(delivery.sequence);
        Ok(())
    }));
    state
        .change(&BACKGROUND_CONTEXT, |draft| {
            draft.set("value", 2)?;
            draft.set("value", 1)
        })
        .unwrap();
    assert_eq!(state.value(), json!({"value": 1}));
    assert_eq!(deliveries.get(), [0]);
}

#[test]
fn hydrates_a_new_subscriber_from_the_latest_atomic_revision() {
    let state = replicated_state(json!({"entries": [{"id": "one"}]}));
    let record = |recorder: &Recorder<Json>| {
        let recorder = recorder.clone();
        StateListener::sync(move |value, _, _| {
            recorder.push(value.clone());
            Ok(())
        })
    };
    let (first, second) = (Recorder::new(), Recorder::new());
    let _a = state.subscribe(record(&first));
    state.change(&BACKGROUND_CONTEXT, |draft| draft.get_mut("entries").unwrap().push(json!({"id": "two"}))).unwrap();
    let _b = state.subscribe(record(&second));
    assert_eq!(first.get(), [json!({"entries": [{"id": "one"}]}), json!({"entries": [{"id": "one"}, {"id": "two"}]})]);
    assert_eq!(second.get(), [json!({"entries": [{"id": "one"}, {"id": "two"}]})]);
}

#[tokio::test]
async fn does_not_defensively_clone_method_arguments_or_results() {
    let echo = define_service::<Echo>("test.echo").unwrap();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&echo)]).unwrap();
    let received: Arc<Mutex<Option<Json>>> = Arc::default();
    let response = json!({"value": "response"});
    let (sink, reply) = (received.clone(), response.clone());
    provider
        .provide(
            &echo,
            ServiceImpl::new().method("echo", move |mut args, _| {
                *sink.lock().unwrap() = Some(args.remove(0));
                let reply = reply.clone();
                async move { Ok(Some(reply)) }
            }),
        )
        .unwrap();
    let namespace = binding(&[&echo], &provider);
    let handle = namespace.use_service(&echo).unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let request = json!({"value": "request"});
    let result = handle.call("echo", vec![request.clone()], &BACKGROUND_CONTEXT).await.unwrap().unwrap();
    assert!(result.same_ref(&response));
    assert!(received.lock().unwrap().as_ref().unwrap().same_ref(&request));
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

#[tokio::test]
async fn provides_and_consumes_one_singleton_with_replicated_state() {
    let models = models();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&models)]).unwrap();
    assert_eq!(provider.catalogue()[0].service_id, "test.models");
    assert_eq!(provider.catalogue()[0].mode, ServiceMode::Singleton);
    let root = json!({"selected": null, "revision": 0});
    let state = replicated_state(root.clone());
    let source = state.clone();
    provider
        .provide(
            &models,
            ServiceImpl::new().state("state", &state).method("select", move |mut args, context| {
                let model = args.remove(0);
                let result = source.change(&context, |draft| {
                    draft.set("selected", model)?;
                    let next = draft.get("revision").and_then(Json::as_f64).unwrap_or(0.0) + 1.0;
                    draft.set("revision", next)
                });
                async move { result.map(|()| None) }
            }),
        )
        .unwrap();
    let (errors, reporter) = error_recorder();
    let namespace = create_remote_service_binding(RemoteServiceBindingOptions {
        on_error: Some(reporter),
        ..RemoteServiceBindingOptions::new(vec![models.erase()], create_loopback_service_transport(&provider))
    })
    .unwrap();
    let first = namespace.use_service(&models).unwrap();
    let second = namespace.use_service(&models).unwrap();
    assert_eq!(first.state("state").unwrap().value().unwrap(), None);
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(first.state("state").unwrap().value().unwrap(), Some(root));

    let updates = Recorder::new();
    let sink = updates.clone();
    let stop = second
        .state("state")
        .unwrap()
        .subscribe(StateListener::sync(move |value, _, _| {
            sink.push(value.clone());
            Ok(())
        }))
        .unwrap();
    let model = json!({"provider": "test", "modelId": "one"});
    first.call("select", vec![model.clone()], &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(first.state("state").unwrap().value().unwrap(), Some(state.value()));
    assert_eq!(
        updates.get(),
        [
            json!({"selected": null, "revision": 0}),
            json!({"selected": {"provider": "test", "modelId": "one"}, "revision": 1})
        ]
    );
    assert!(errors.get().is_empty());

    let late = binding(&[&models], &provider);
    let late_models = late.use_service(&models).unwrap();
    late.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(revision(late_models.state("state").unwrap().value().unwrap()), Some(1.0));
    stop.unsubscribe();
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    late.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

#[tokio::test]
async fn publishes_compact_tracked_operations_through_the_remote_provider() {
    let timeline = define_service::<Timeline>("test.timeline").unwrap();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&timeline)]).unwrap();
    let root = json!({"entries": [{"id": "one"}], "retained": {"value": 1}});
    let source = replicated_state(root.clone());
    provider.provide(&timeline, ServiceImpl::new().state("state", &source)).unwrap();
    let updates = Recorder::new();
    let raw = provider.subscribe("test.timeline", ServiceMode::Singleton, updates_listener(&updates)).unwrap();
    assert_eq!(
        raw.snapshot().to_json()["instances"][0]["members"],
        json!([{"name": "state", "kind": "state", "sequence": 0, "ops": [["r", root]]}])
    );
    raw.activate().unwrap();

    let namespace = binding(&[&timeline], &provider);
    let handle = namespace.use_service(&timeline).unwrap();
    namespace.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let state = handle.state("state").unwrap();
    let previous = state.value().unwrap().unwrap();
    source.change(&BACKGROUND_CONTEXT, |draft| draft.get_mut("entries").unwrap().push(json!({"id": "two"}))).unwrap();
    assert!(updates.get().contains(&json!({
        "type": "state", "member": "state", "sequence": 1, "ops": [["p", ["entries"], 1, 0, [{"id": "two"}]]]
    })));
    assert_eq!(previous, json!({"entries": [{"id": "one"}], "retained": {"value": 1}}));
    let next = state.value().unwrap().unwrap();
    assert_eq!(next, json!({"entries": [{"id": "one"}, {"id": "two"}], "retained": {"value": 1}}));
    assert!(next["retained"].same_ref(&previous["retained"]));

    source.change(&BACKGROUND_CONTEXT, |draft| draft.get_mut("entries").unwrap().push(json!({"id": "three"}))).unwrap();
    let late = provider.subscribe("test.timeline", ServiceMode::Singleton, Arc::new(|_, _| Ok(()))).unwrap();
    assert_eq!(
        updates.get().last().unwrap(),
        &json!({"type": "state", "member": "state", "sequence": 2, "ops": [["p", ["entries"], 2, 0, [{"id": "three"}]]]})
    );
    let full = json!({"entries": [{"id": "one"}, {"id": "two"}, {"id": "three"}], "retained": {"value": 1}});
    assert_eq!(
        late.snapshot().to_json()["instances"][0]["members"],
        json!([{"name": "state", "kind": "state", "sequence": 2, "ops": [["r", full]]}])
    );
    late.close();
    raw.close();
    namespace.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    provider.dispose().unwrap();
}

type FrameSink = Arc<Mutex<Option<Arc<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>>>>;

struct FixedSource {
    value: Json,
    publish: FrameSink,
    disposed: Arc<Mutex<bool>>,
}

struct FixedAttachment {
    snapshot: ReplicatedStateSourceSnapshot,
    publish: FrameSink,
    disposed: Arc<Mutex<bool>>,
}

impl ReplicatedStateSource for FixedSource {
    fn attach(&self) -> pi_chord::ChordResult<Box<dyn ReplicatedStateSourceAttachment>> {
        Ok(Box::new(FixedAttachment {
            snapshot: ReplicatedStateSourceSnapshot { value: self.value.clone(), cursor: 20 },
            publish: self.publish.clone(),
            disposed: self.disposed.clone(),
        }))
    }
}

impl ReplicatedStateSourceAttachment for FixedAttachment {
    fn snapshot(&self) -> ReplicatedStateSourceSnapshot {
        self.snapshot.clone()
    }

    fn activate(&self, listener: Box<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>) -> pi_chord::ChordResult<()> {
        *self.publish.lock().unwrap() = Some(Arc::from(listener));
        Ok(())
    }

    fn dispose(&self) -> pi_chord::ChordResult<()> {
        *self.disposed.lock().unwrap() = true;
        Ok(())
    }
}

#[test]
fn publishes_authoritative_source_references_through_services_without_re_diffing() {
    let timeline = define_service::<Timeline>("test.timeline").unwrap();
    let root = json!({"entries": [{"id": "one"}], "retained": {"value": 1}});
    let source = FixedSource { value: root.clone(), publish: Arc::default(), disposed: Arc::default() };
    let state = attach_replicated_state_source(&source, ReplicatedStateSourceOptions::default()).unwrap();
    let provider = RemoteServiceProvider::new(&[ServiceDefinition::singleton(&timeline)]).unwrap();
    provider.provide(&timeline, ServiceImpl::new().state("state", &state)).unwrap();
    let updates: Arc<Mutex<Vec<ServiceProviderUpdate>>> = Arc::default();
    let sink = updates.clone();
    let subscription = provider
        .subscribe(
            "test.timeline",
            ServiceMode::Singleton,
            Arc::new(move |update, _| {
                sink.lock().unwrap().push(update);
                Ok(())
            }),
        )
        .unwrap();
    let ServiceMemberSnapshotOf::State { ops, .. } = &subscription.snapshot().instances[0].members[0] else {
        panic!("expected a state member");
    };
    let Op::Replace(value) = &ops[0] else { panic!("expected a root replacement") };
    assert!(value.same_ref(&root));
    subscription.activate().unwrap();

    let next = json!({"entries": [{"id": "one"}, {"id": "two"}], "retained": root["retained"].clone()});
    let batch = pi_chord::delta::parse_ops(&json!([["p", ["entries"], 1, 0, [{"id": "two"}]]])).unwrap();
    let publish = source.publish.lock().unwrap().clone().unwrap();
    publish(ReplicatedStateSourceFrame {
        cursor: 21,
        value: next.clone(),
        ops: batch.clone(),
        context: BACKGROUND_CONTEXT.clone(),
    });
    let updates = updates.lock().unwrap().clone();
    assert_eq!(updates.len(), 1);
    match &updates[0] {
        pi_chord::services::ServiceProviderUpdateOf::State { ops, .. } => assert_eq!(ops, &batch),
        other => panic!("unexpected update {other:?}"),
    }
    assert!(state.value().same_ref(&next));
    subscription.close();
    provider.dispose().unwrap();
    state.dispose().unwrap();
    assert!(*source.disposed.lock().unwrap());
}
