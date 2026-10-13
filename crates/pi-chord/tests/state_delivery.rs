//! Port of upstream `test/state-delivery.test.ts`.

mod support;

use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;
use pi_chord::state::ErrorReporter;
use pi_chord::{
    BACKGROUND_CONTEXT, ChordError, ChordResult, Context, DeliveryKind, Json, ReplicatedState, ReplicatedStateDelivery,
    ReplicatedStateReplica, ReplicatedStateSource, ReplicatedStateSourceAttachment, ReplicatedStateSourceFrame,
    ReplicatedStateSourceOptions, ReplicatedStateSourceSnapshot, StateListener, attach_replicated_state_source, json,
    path, replicated_state, set_unhandled_error_hook, with_cancel,
};
use support::{Gate, Recorder, settle, wait_for};

#[derive(Clone, Copy, Debug)]
enum Kind {
    Mutable,
    Attached,
    Replica,
}

const KINDS: [Kind; 3] = [Kind::Mutable, Kind::Attached, Kind::Replica];

type Publish = Arc<dyn Fn(i64, &Context) + Send + Sync>;
type FrameListener = Arc<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>;

struct Fixture {
    state: Arc<dyn ReplicatedState>,
    publish: Publish,
    exact: Option<Recorder<u64>>,
}

struct OneShotSource(Arc<Mutex<Option<FrameListener>>>);
struct OneShotAttachment(Arc<Mutex<Option<FrameListener>>>);

impl ReplicatedStateSource for OneShotSource {
    fn attach(&self) -> ChordResult<Box<dyn ReplicatedStateSourceAttachment>> {
        Ok(Box::new(OneShotAttachment(self.0.clone())))
    }
}

impl ReplicatedStateSourceAttachment for OneShotAttachment {
    fn snapshot(&self) -> ReplicatedStateSourceSnapshot {
        ReplicatedStateSourceSnapshot { value: json!({"value": 0}), cursor: 0 }
    }
    fn activate(&self, listener: Box<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>) -> ChordResult<()> {
        *self.0.lock().unwrap() = Some(Arc::from(listener));
        Ok(())
    }
    fn dispose(&self) -> ChordResult<()> {
        Ok(())
    }
}

fn value_of(number: i64) -> Json {
    json!({ "value": number })
}

fn fixture(kind: Kind, on_error: Option<ErrorReporter>) -> Fixture {
    let on_error = on_error.unwrap_or_else(|| Arc::new(|_| {}));
    match kind {
        Kind::Mutable => {
            let state = replicated_state(value_of(0));
            let exact = Recorder::new();
            let recorder = exact.clone();
            std::mem::forget(state.member().subscribe_ops(move |_, sequence, _| {
                recorder.push(sequence);
                Ok(())
            }));
            let inner = state.clone();
            let publish: Publish = Arc::new(move |value, context| inner.replace(context, value_of(value)).unwrap());
            Fixture { state: Arc::new(state), publish, exact: Some(exact) }
        }
        Kind::Attached => {
            let receive: Arc<Mutex<Option<FrameListener>>> = Arc::default();
            let options = ReplicatedStateSourceOptions { on_error: Some(on_error) };
            let state = attach_replicated_state_source(&OneShotSource(receive.clone()), options).unwrap();
            let exact = Recorder::new();
            let recorder = exact.clone();
            std::mem::forget(state.member().subscribe_ops(move |_, sequence, _| {
                recorder.push(sequence);
                Ok(())
            }));
            let publish: Publish = Arc::new(move |value, context| {
                let listener = receive.lock().unwrap().clone().unwrap();
                listener(ReplicatedStateSourceFrame {
                    cursor: value,
                    value: value_of(value),
                    ops: vec![Op::Set(path!["value"], value.into())],
                    context: context.clone(),
                });
            });
            Fixture { state: Arc::new(state), publish, exact: Some(exact) }
        }
        Kind::Replica => {
            let replica = ReplicatedStateReplica::new(on_error);
            replica.hydrate(0, &[Op::Replace(value_of(0))], &BACKGROUND_CONTEXT).unwrap();
            let inner = replica.clone();
            let publish: Publish = Arc::new(move |value, context| {
                inner.update(value as u64, &[Op::Set(path!["value"], value.into())], context).unwrap();
            });
            Fixture { state: Arc::new(replica), publish, exact: None }
        }
    }
}

fn number(value: &Json) -> i64 {
    value.get("value").unwrap().as_i64().unwrap()
}

/// A listener that records each value and waits on `gate` for the values `gated` selects.
fn gated(received: &Recorder<i64>, gate: &Gate, gated: fn(i64) -> bool) -> StateListener {
    let (received, gate) = (received.clone(), gate.clone());
    StateListener::asynchronous(move |value, _, _| {
        let value = number(&value);
        received.push(value);
        let gate = gated(value).then(|| gate.clone());
        async move {
            match gate {
                Some(gate) => gate.wait().await,
                None => Ok(()),
            }
        }
    })
}

fn fast(received: &Recorder<i64>) -> StateListener {
    let received = received.clone();
    StateListener::sync(move |value, _, _| {
        received.push(number(value));
        Ok(())
    })
}

#[tokio::test]
async fn awaits_hydration_and_each_update_independently() {
    for kind in KINDS {
        let Fixture { state, publish, exact } = fixture(kind, None);
        let (hydration, update) = (Gate::new(), Gate::new());
        let events: Recorder<String> = Recorder::new();
        let fast_values = Recorder::new();
        let (recorder, first, second) = (events.clone(), hydration.clone(), update.clone());
        let _a = state.subscribe(StateListener::asynchronous(move |value, _, _| {
            let (events, first, second) = (recorder.clone(), first.clone(), second.clone());
            async move {
                let value = number(&value);
                events.push(format!("start:{value}"));
                if value == 0 {
                    first.wait().await?;
                }
                if value == 1 {
                    second.wait().await?;
                }
                events.push(format!("end:{value}"));
                Ok(())
            }
        }));
        let _b = state.subscribe(fast(&fast_values));
        publish(1, &BACKGROUND_CONTEXT);
        publish(2, &BACKGROUND_CONTEXT);
        assert_eq!(events.get(), ["start:0"], "{kind:?}");
        assert_eq!(fast_values.get(), [0, 1, 2]);
        assert_eq!(state.value(), Some(value_of(2)));
        if let Some(exact) = exact {
            assert_eq!(exact.get(), [1, 2]);
        }
        hydration.resolve();
        wait_for(|| events.get() == ["start:0", "end:0", "start:1"]).await;
        update.resolve();
        wait_for(|| events.get() == ["start:0", "end:0", "start:1", "end:1", "start:2", "end:2"]).await;
    }
}

#[tokio::test]
async fn bounds_pending_deliveries_without_changing_exact_publication() {
    for kind in KINDS {
        for count in [100i64, 101, 102, 201, 202] {
            let Fixture { state, publish, exact } = fixture(kind, None);
            let hydration = Gate::new();
            let received = Recorder::new();
            let _stop = state.subscribe(gated(&received, &hydration, |value| value == 0));
            for value in 1..=count {
                publish(value, &BACKGROUND_CONTEXT);
            }
            assert_eq!(received.get(), [0]);
            if let Some(exact) = exact {
                assert_eq!(exact.get(), (1..=count as u64).collect::<Vec<_>>());
            }
            hydration.resolve();
            let first = (count - 1) / 100 * 100 + 1;
            let expected: Vec<i64> = std::iter::once(0).chain(first..=count).collect();
            wait_for(|| received.get() == expected).await;
        }
    }
}

#[tokio::test]
async fn excludes_a_running_update_from_overflow_and_keeps_exact_metadata() {
    for kind in KINDS {
        let Fixture { state, publish, .. } = fixture(kind, None);
        let gate = Gate::new();
        let (context, _controller) = with_cancel(&BACKGROUND_CONTEXT);
        let received: Recorder<(Json, Context, ReplicatedStateDelivery)> = Recorder::new();
        let (recorder, waiting) = (received.clone(), gate.clone());
        let _stop = state.subscribe(StateListener::asynchronous(move |value, context, delivery| {
            recorder.push((value.clone(), context, delivery));
            let gate = (number(&value) == 1).then(|| waiting.clone());
            async move {
                match gate {
                    Some(gate) => gate.wait().await,
                    None => Ok(()),
                }
            }
        }));
        for value in 1..102 {
            publish(value, &BACKGROUND_CONTEXT);
        }
        publish(102, &context);
        let adopted = state.value().unwrap();
        publish(103, &BACKGROUND_CONTEXT);
        let values = |list: Vec<(Json, Context, ReplicatedStateDelivery)>| -> Vec<i64> {
            list.iter().map(|(value, ..)| number(value)).collect()
        };
        assert_eq!(values(received.get()), [0, 1]);
        gate.resolve();
        wait_for(|| values(received.get()) == [0, 1, 102, 103]).await;
        let (value, delivered_context, delivery) = received.get()[2].clone();
        assert!(value.same_ref(&adopted), "{kind:?}");
        assert!(delivered_context.ptr_eq(&context));
        assert_eq!(delivery, ReplicatedStateDelivery { kind: DeliveryKind::Update, sequence: 102 });
    }
}

#[test]
fn serializes_reentrant_hydration_and_update_callbacks() {
    for kind in KINDS {
        let Fixture { state, publish, .. } = fixture(kind, None);
        let events: Recorder<String> = Recorder::new();
        let recorder = events.clone();
        let _stop = state.subscribe(StateListener::sync(move |value, _, _| {
            let value = number(value);
            recorder.push(format!("start:{value}"));
            if value < 2 {
                publish(value + 1, &BACKGROUND_CONTEXT);
            }
            recorder.push(format!("end:{value}"));
            Ok(())
        }));
        assert_eq!(events.get(), ["start:0", "end:0", "start:1", "end:1", "start:2", "end:2"], "{kind:?}");
    }
}

#[tokio::test]
async fn treats_two_subscriptions_of_the_same_callback_independently() {
    for kind in KINDS {
        let Fixture { state, publish, .. } = fixture(kind, None);
        let gate = Gate::new();
        let received = Recorder::new();
        let listener = gated(&received, &gate, |value| value == 0);
        let stop_first = state.subscribe(listener.clone());
        let stop_second = state.subscribe(listener);
        publish(1, &BACKGROUND_CONTEXT);
        stop_first.unsubscribe();
        stop_first.unsubscribe();
        gate.resolve();
        wait_for(|| received.get() == [0, 0, 1]).await;
        settle().await;
        assert_eq!(received.get(), [0, 0, 1]);
        stop_second.unsubscribe();
    }
}

#[tokio::test]
async fn unsubscribe_drops_queued_callbacks_without_joining_or_aborting_the_running_one() {
    for kind in KINDS {
        let Fixture { state, publish, .. } = fixture(kind, None);
        let gate = Gate::new();
        let (context, _controller) = with_cancel(&BACKGROUND_CONTEXT);
        let received = Recorder::new();
        let completed = Arc::new(Mutex::new(false));
        let (recorder, waiting, done) = (received.clone(), gate.clone(), completed.clone());
        let stop = state.subscribe(StateListener::asynchronous(move |value, _, _| {
            let value = number(&value);
            recorder.push(value);
            let (gate, done) = (waiting.clone(), done.clone());
            async move {
                if value == 1 {
                    gate.wait().await?;
                    *done.lock().unwrap() = true;
                }
                Ok(())
            }
        }));
        settle().await;
        publish(1, &context);
        publish(2, &context);
        stop.unsubscribe();
        publish(3, &context);
        assert!(!context.is_cancelled());
        assert!(!*completed.lock().unwrap());
        gate.resolve();
        wait_for(|| *completed.lock().unwrap()).await;
        assert_eq!(received.get(), [0, 1]);
    }
}

fn reporter(errors: &Recorder<String>) -> ErrorReporter {
    let errors = errors.clone();
    Arc::new(move |error: ChordError| errors.push(error.to_string()))
}

#[test]
fn isolates_a_synchronous_hydration_failure_without_removing_the_subscription() {
    for kind in [Kind::Attached, Kind::Replica] {
        let errors = Recorder::new();
        let Fixture { state, publish, .. } = fixture(kind, Some(reporter(&errors)));
        let received = Recorder::new();
        let recorder = received.clone();
        let _stop = state.subscribe(StateListener::sync(move |value, _, _| {
            let value = number(value);
            recorder.push(value);
            if value == 0 { Err(ChordError::error("sync hydration")) } else { Ok(()) }
        }));
        publish(1, &BACKGROUND_CONTEXT);
        assert_eq!(errors.get(), ["sync hydration"]);
        assert_eq!(received.get(), [0, 1]);
    }
}

#[tokio::test]
async fn observes_rejections_and_throws_while_continuing_delivery() {
    for kind in [Kind::Attached, Kind::Replica] {
        let errors = Recorder::new();
        let Fixture { state, publish, .. } = fixture(kind, Some(reporter(&errors)));
        let gate = Gate::new();
        let received = Recorder::new();
        let fast_values = Recorder::new();
        let (recorder, waiting) = (received.clone(), gate.clone());
        let _a = state.subscribe(StateListener::asynchronous(move |value, _, _| {
            let value = number(&value);
            recorder.push(value);
            let gate = waiting.clone();
            async move {
                match value {
                    0 => gate.wait().await,
                    1 => Err(ChordError::error("sync update")),
                    2 => Err(ChordError::error("async update")),
                    _ => Ok(()),
                }
            }
        }));
        let _b = state.subscribe(fast(&fast_values));
        for value in 1..=3 {
            publish(value, &BACKGROUND_CONTEXT);
        }
        gate.reject("async hydration");
        wait_for(|| received.get() == [0, 1, 2, 3]).await;
        assert_eq!(errors.get(), ["async hydration", "sync update", "async update"]);
        assert_eq!(fast_values.get(), [0, 1, 2, 3]);
    }
}

#[tokio::test]
async fn still_observes_a_callback_rejection_after_unsubscribe() {
    for kind in [Kind::Attached, Kind::Replica] {
        let errors = Recorder::new();
        let Fixture { state, publish, .. } = fixture(kind, Some(reporter(&errors)));
        let gate = Gate::new();
        let received = Recorder::new();
        let stop = state.subscribe(gated(&received, &gate, |_| true));
        publish(1, &BACKGROUND_CONTEXT);
        stop.unsubscribe();
        gate.reject("stopped callback");
        wait_for(|| errors.get() == ["stopped callback"]).await;
        assert_eq!(received.get(), [0]);
    }
}

#[tokio::test]
async fn mutable_state_reports_rejected_callbacks_through_its_default_reporter() {
    let reports = Recorder::new();
    let recorder = reports.clone();
    set_unhandled_error_hook(Some(Arc::new(move |error: &ChordError| recorder.push(error.to_string()))));
    let state = replicated_state(value_of(0));
    let received = Recorder::new();
    let recorder = received.clone();
    let _stop = state.subscribe(StateListener::asynchronous(move |value, _, _| {
        let value = number(&value);
        recorder.push(value);
        async move { if value == 0 { Err(ChordError::error("async mutable hydration")) } else { Ok(()) } }
    }));
    state.replace(&BACKGROUND_CONTEXT, value_of(1)).unwrap();
    settle().await;
    assert_eq!(received.get(), [0, 1]);
    assert_eq!(reports.get(), ["async mutable hydration"]);
    set_unhandled_error_hook(None);
}

#[tokio::test]
async fn replica_disconnect_drops_obsolete_work_but_waits_for_the_running_callback() {
    let replica = ReplicatedStateReplica::new(Arc::new(|_| {}));
    let gate = Gate::new();
    let received: Recorder<ReplicatedStateDelivery> = Recorder::new();
    let (recorder, waiting) = (received.clone(), gate.clone());
    let _stop = replica.subscribe(StateListener::asynchronous(move |_, _, delivery| {
        recorder.push(delivery);
        let gate = (delivery.sequence == 0).then(|| waiting.clone());
        async move {
            match gate {
                Some(gate) => gate.wait().await,
                None => Ok(()),
            }
        }
    }));
    replica.hydrate(0, &[Op::Replace(value_of(0))], &BACKGROUND_CONTEXT).unwrap();
    replica.update(1, &[Op::Set(path!["value"], 1.into())], &BACKGROUND_CONTEXT).unwrap();
    replica.clear();
    replica.hydrate(50, &[Op::Replace(value_of(50))], &BACKGROUND_CONTEXT).unwrap();
    replica.update(51, &[Op::Set(path!["value"], 51.into())], &BACKGROUND_CONTEXT).unwrap();
    assert_eq!(received.len(), 1);
    gate.resolve();
    wait_for(|| received.len() == 3).await;
    let deliveries: Vec<_> = received.get().iter().map(|delivery| (delivery.kind, delivery.sequence)).collect();
    assert_eq!(deliveries, [(DeliveryKind::Hydrate, 0), (DeliveryKind::Hydrate, 50), (DeliveryKind::Update, 51)]);
}

#[test]
fn cold_replicas_hydrate_all_listeners_before_their_reentrant_updates() {
    let replica = ReplicatedStateReplica::new(Arc::new(|_| {}));
    let second: Recorder<ReplicatedStateDelivery> = Recorder::new();
    let inner = replica.clone();
    let _a = replica.subscribe(StateListener::sync(move |_, _, delivery| {
        if delivery.kind == DeliveryKind::Hydrate {
            inner.update(1, &[Op::Set(path!["value"], 1.into())], &BACKGROUND_CONTEXT)?;
        }
        Ok(())
    }));
    let recorder = second.clone();
    let _b = replica.subscribe(StateListener::sync(move |_, _, delivery| {
        recorder.push(delivery);
        Ok(())
    }));
    replica.hydrate(0, &[Op::Replace(value_of(0))], &BACKGROUND_CONTEXT).unwrap();
    let deliveries: Vec<_> = second.get().iter().map(|delivery| (delivery.kind, delivery.sequence)).collect();
    assert_eq!(deliveries, [(DeliveryKind::Hydrate, 0), (DeliveryKind::Update, 1)]);
}
