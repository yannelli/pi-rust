//! Port of upstream `test/state.test.ts`, authoritative replicated state sources.

mod support;

use std::sync::{Arc, Mutex, Weak};

use pi_chord::delta::parse_ops;
use pi_chord::{
    BACKGROUND_CONTEXT, ChordError, ChordResult, DeliveryKind, Json, ReplicatedStateSource,
    ReplicatedStateSourceAttachment, ReplicatedStateSourceFrame, ReplicatedStateSourceOptions,
    ReplicatedStateSourceSnapshot, StateListener, attach_replicated_state_source, json,
};
use support::Recorder;

type Hook = Box<dyn Fn(&Arc<TestSource>) + Send + Sync>;
type FrameListener = Arc<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>;

#[derive(Default)]
struct SourceState {
    value: Json,
    cursor: i64,
    attachments: Vec<Arc<TestAttachment>>,
}

#[derive(Default)]
pub struct TestSource {
    state: Mutex<SourceState>,
    on_attach: Mutex<Option<Hook>>,
    me: Mutex<Option<Weak<TestSource>>>,
}

impl TestSource {
    fn new(value: Json, cursor: i64) -> Arc<Self> {
        let source = Arc::new(Self::default());
        *source.state.lock().unwrap() = SourceState { value, cursor, attachments: Vec::new() };
        *source.me.lock().unwrap() = Some(Arc::downgrade(&source));
        source
    }

    fn attachments(&self) -> usize {
        self.state.lock().unwrap().attachments.len()
    }

    fn commit(&self, value: Json, batch: Json, cursor: Option<i64>) {
        let (attachments, cursor) = {
            let mut state = self.state.lock().unwrap();
            state.value = value.clone();
            state.cursor = cursor.unwrap_or(state.cursor + 1);
            (state.attachments.clone(), state.cursor)
        };
        let ops = parse_ops(&batch).unwrap();
        let frame = ReplicatedStateSourceFrame { cursor, value, ops, context: BACKGROUND_CONTEXT.clone() };
        for attachment in attachments {
            attachment.publish(frame.clone());
        }
    }
}

#[derive(Default)]
struct TestAttachment {
    snapshot: Mutex<Option<ReplicatedStateSourceSnapshot>>,
    buffer: Mutex<Vec<ReplicatedStateSourceFrame>>,
    listener: Mutex<Option<FrameListener>>,
    disposed: Mutex<bool>,
    source: Mutex<Option<Weak<TestSource>>>,
}

impl TestAttachment {
    fn publish(&self, frame: ReplicatedStateSourceFrame) {
        if *self.disposed.lock().unwrap() {
            return;
        }
        let listener = self.listener.lock().unwrap().clone();
        match listener {
            Some(listener) => listener(frame),
            None => self.buffer.lock().unwrap().push(frame),
        }
    }
}

struct AttachmentHandle(Arc<TestAttachment>);

struct SourceHandle(Arc<TestSource>);

impl ReplicatedStateSource for SourceHandle {
    fn attach(&self) -> ChordResult<Box<dyn ReplicatedStateSourceAttachment>> {
        let attachment = Arc::new(TestAttachment::default());
        {
            let mut state = self.0.state.lock().unwrap();
            *attachment.snapshot.lock().unwrap() =
                Some(ReplicatedStateSourceSnapshot { value: state.value.clone(), cursor: state.cursor });
            *attachment.source.lock().unwrap() = self.0.me.lock().unwrap().clone();
            state.attachments.push(attachment.clone());
        }
        let hook = self.0.on_attach.lock().unwrap().take();
        if let Some(hook) = hook {
            hook(&self.0);
        }
        Ok(Box::new(AttachmentHandle(attachment)))
    }
}

impl ReplicatedStateSourceAttachment for AttachmentHandle {
    fn snapshot(&self) -> ReplicatedStateSourceSnapshot {
        self.0.snapshot.lock().unwrap().clone().unwrap()
    }

    fn activate(&self, listener: Box<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>) -> ChordResult<()> {
        let listener: FrameListener = Arc::from(listener);
        *self.0.listener.lock().unwrap() = Some(listener.clone());
        let buffered = std::mem::take(&mut *self.0.buffer.lock().unwrap());
        for frame in buffered {
            listener(frame);
        }
        Ok(())
    }

    fn dispose(&self) -> ChordResult<()> {
        {
            let mut disposed = self.0.disposed.lock().unwrap();
            if *disposed {
                return Ok(());
            }
            *disposed = true;
        }
        self.0.buffer.lock().unwrap().clear();
        *self.0.listener.lock().unwrap() = None;
        let source = self.0.source.lock().unwrap().clone().and_then(|weak| weak.upgrade());
        if let Some(source) = source {
            source.state.lock().unwrap().attachments.retain(|other| !Arc::ptr_eq(other, &self.0));
        }
        Ok(())
    }
}

fn attach(source: &Arc<TestSource>, options: ReplicatedStateSourceOptions) -> pi_chord::AttachedReplicatedState {
    attach_replicated_state_source(&SourceHandle(source.clone()), options).unwrap()
}

fn number(value: &Json) -> i64 {
    value.get("value").unwrap().as_i64().unwrap()
}

#[test]
fn captures_before_activation_and_drains_queued_commits_in_order() {
    let source = TestSource::new(json!({"value": 0}), 10);
    *source.on_attach.lock().unwrap() = Some(Box::new(|source| {
        source.commit(json!({"value": 1}), json!([["s", ["value"], 1]]), None);
        source.commit(json!({"value": 2}), json!([["s", ["value"], 2]]), None);
    }));
    let state = attach(&source, ReplicatedStateSourceOptions::default());
    let deliveries = Recorder::new();
    let recorder = deliveries.clone();
    let _stop = state.subscribe(StateListener::sync(move |value, _, delivery| {
        recorder.push((value.clone(), delivery.sequence));
        Ok(())
    }));
    assert_eq!(state.value(), json!({"value": 2}));
    assert_eq!(deliveries.get(), [(json!({"value": 2}), 2)]);
    assert_eq!(state.member().snapshot(), (json!({"value": 2}), 2));
}

#[test]
fn hydrates_at_sequence_zero_when_attaching_after_existing_commits() {
    let source = TestSource::new(json!({"value": 0}), 40);
    source.commit(json!({"value": 1}), json!([["s", ["value"], 1]]), None);
    let state = attach(&source, ReplicatedStateSourceOptions::default());
    let deliveries = Recorder::new();
    let recorder = deliveries.clone();
    let _stop = state.subscribe(StateListener::sync(move |_, _, delivery| {
        recorder.push(delivery.sequence);
        Ok(())
    }));
    assert_eq!(state.value(), json!({"value": 1}));
    assert_eq!(deliveries.get(), [0]);
}

#[test]
fn publishes_exact_source_values_without_applying_or_rediffing() {
    let source = TestSource::new(json!({"value": 0}), 0);
    let state = attach(&source, ReplicatedStateSourceOptions::default());
    let published = Recorder::new();
    let recorder = published.clone();
    let _stop = state.subscribe(StateListener::sync(move |value, _, delivery| {
        if delivery.kind == DeliveryKind::Update {
            recorder.push(value.clone());
        }
        Ok(())
    }));
    let next = json!({"value": 1});
    source.commit(next.clone(), json!([["s", ["value"], 1]]), None);
    assert!(state.value().same_ref(&next));
    assert!(published.get()[0].same_ref(&next));
}

#[test]
fn buffers_reentrant_frames_and_skips_updates_covered_by_a_late_hydration() {
    let source = TestSource::new(json!({"value": 0}), 0);
    let state = attach(&source, ReplicatedStateSourceOptions::default());
    let received = Recorder::new();
    let late = Recorder::new();
    let nested = Arc::new(Mutex::new(false));
    let (inner_source, inner_state, late_recorder) = (source.clone(), state.clone(), late.clone());
    let _a = state.member().subscribe_ops(move |_, sequence, _| {
        {
            let mut flag = nested.lock().unwrap();
            if sequence != 1 || *flag {
                return Ok(());
            }
            *flag = true;
        }
        inner_source.commit(json!({"value": 2}), json!([["s", ["value"], 2]]), None);
        let late = late_recorder.clone();
        std::mem::forget(inner_state.subscribe(StateListener::sync(move |value, _, delivery| {
            late.push((delivery.kind, delivery.sequence, number(value)));
            Ok(())
        })));
        Ok(())
    });
    let recorder = received.clone();
    let _b = state.subscribe(StateListener::sync(move |value, _, delivery| {
        if delivery.kind == DeliveryKind::Update {
            recorder.push(number(value));
        }
        Ok(())
    }));
    source.commit(json!({"value": 1}), json!([["s", ["value"], 1]]), None);
    assert_eq!(received.get(), [1, 2]);
    assert_eq!(late.get(), [(DeliveryKind::Hydrate, 2, 2)]);
}

fn recording_options(errors: &Recorder<String>) -> ReplicatedStateSourceOptions {
    let recorder = errors.clone();
    ReplicatedStateSourceOptions { on_error: Some(Arc::new(move |error| recorder.push(error.to_string()))) }
}

#[test]
fn reports_listener_failures_without_throwing_them_into_the_source() {
    let source = TestSource::new(json!({"value": 0}), 0);
    let errors = Recorder::new();
    let state = attach(&source, recording_options(&errors));
    let received = Recorder::new();
    let _a = state.subscribe(StateListener::sync(|_, _, delivery| {
        if delivery.kind == DeliveryKind::Update { Err(ChordError::error("listener failed")) } else { Ok(()) }
    }));
    let recorder = received.clone();
    let _b = state.subscribe(StateListener::sync(move |value, _, delivery| {
        if delivery.kind == DeliveryKind::Update {
            recorder.push(number(value));
        }
        Ok(())
    }));
    source.commit(json!({"value": 1}), json!([["s", ["value"], 1]]), None);
    source.commit(json!({"value": 2}), json!([["s", ["value"], 2]]), None);
    assert_eq!(received.get(), [1, 2]);
    assert_eq!(errors.get(), ["listener failed", "listener failed"]);
}

#[test]
fn reports_cursor_gaps_disposes_the_broken_attachment_and_ignores_later_frames() {
    let source = TestSource::new(json!({"value": 0}), 5);
    let errors = Recorder::new();
    let state = attach(&source, recording_options(&errors));
    source.commit(json!({"value": 2}), json!([["s", ["value"], 2]]), Some(7));
    assert!(errors.get()[0].contains("expected 6, received 7"));
    assert_eq!(source.attachments(), 0);
    assert_eq!(state.value(), json!({"value": 0}));
    source.commit(json!({"value": 3}), json!([["s", ["value"], 3]]), Some(8));
    assert_eq!(state.value(), json!({"value": 0}));
}

#[test]
fn keeps_attachments_independent_and_disposes_each_idempotently() {
    let source = TestSource::new(json!({"value": 0}), 0);
    let first = attach(&source, ReplicatedStateSourceOptions::default());
    let second = attach(&source, ReplicatedStateSourceOptions::default());
    assert_eq!(source.attachments(), 2);
    source.commit(json!({"value": 1}), json!([["s", ["value"], 1]]), None);
    assert_eq!(first.value(), json!({"value": 1}));
    assert_eq!(second.value(), json!({"value": 1}));
    first.dispose().unwrap();
    first.dispose().unwrap();
    assert_eq!(source.attachments(), 1);
    source.commit(json!({"value": 2}), json!([["s", ["value"], 2]]), None);
    assert_eq!(first.value(), json!({"value": 1}));
    assert_eq!(second.value(), json!({"value": 2}));
    second.dispose().unwrap();
    assert_eq!(source.attachments(), 0);
}
