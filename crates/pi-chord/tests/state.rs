//! Port of upstream `test/state.test.ts`, mutable state and replicas. Sources are in `state_source.rs`.

mod support;

use std::sync::{Arc, Mutex};

use pi_chord::delta::{Op, parse_ops};
use pi_chord::state::ErrorReporter;
use pi_chord::{
    BACKGROUND_CONTEXT, ChordError, Context, DeliveryKind, Json, ReplicatedStateReplica, StateListener, json, path,
    replicated_state,
};
use support::Recorder;

fn ops(value: Json) -> Vec<Op> {
    parse_ops(&value).unwrap()
}

fn ctx() -> Context {
    BACKGROUND_CONTEXT.clone()
}

fn sequences(recorder: &Recorder<u64>) -> StateListener {
    let recorder = recorder.clone();
    StateListener::sync(move |_, _, delivery| {
        recorder.push(delivery.sequence);
        Ok(())
    })
}

#[test]
fn publishes_one_immutable_structurally_shared_revision() {
    let state = replicated_state(json!({"changed": {"value": 1}, "retained": {"value": 2}}));
    let previous = state.value();
    let deliveries = Recorder::new();
    let _stop = state.subscribe(sequences(&deliveries));
    state
        .change(&ctx(), |draft| {
            draft.get_mut("changed").unwrap().set("value", 3)?;
            draft.get_mut("changed").unwrap().set("value", 4)
        })
        .unwrap();
    assert_eq!(state.value(), json!({"changed": {"value": 4}, "retained": {"value": 2}}));
    assert!(!state.value().same_ref(&previous));
    assert!(!state.value().get("changed").unwrap().same_ref(previous.get("changed").unwrap()));
    assert!(state.value().get("retained").unwrap().same_ref(previous.get("retained").unwrap()));
    assert_eq!(deliveries.get(), [0, 1]);
}

#[test]
fn rolls_back_callback_failures() {
    let state = replicated_state(json!({"nested": {"value": 1}}));
    let previous = state.value();
    let error = state
        .change(&ctx(), |draft| {
            draft.get_mut("nested").unwrap().set("value", 2)?;
            Err(ChordError::error("stop"))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "stop");
    assert!(state.value().same_ref(&previous));
}

#[test]
fn rejects_nested_changes_and_replacements_without_losing_the_outer_rollback() {
    let state = replicated_state(json!({"left": 0, "right": 0}));
    let inner = state.clone();
    let error = state
        .change(&ctx(), |draft| {
            draft.set("left", 1)?;
            inner.change(&ctx(), |nested| nested.set("right", 2))
        })
        .unwrap_err();
    assert!(error.to_string().contains("reentrantly"));
    assert_eq!(state.value(), json!({"left": 0, "right": 0}));
    let inner = state.clone();
    let error = state.change(&ctx(), |_| inner.replace(&ctx(), json!({"left": 1, "right": 2}))).unwrap_err();
    assert!(error.to_string().contains("change callback"));
    assert_eq!(state.value(), json!({"left": 0, "right": 0}));
}

#[test]
fn queues_listener_triggered_changes_in_sequence_order() {
    let state = replicated_state(json!({"value": 0}));
    let source_sequences = Recorder::new();
    let deliveries = Recorder::new();
    let late = Recorder::new();
    let nested = Arc::new(Mutex::new(false));
    let (inner, late_inner) = (state.clone(), late.clone());
    let _a = state.member().subscribe_ops(move |_, _, _| {
        let mut flag = nested.lock().unwrap();
        if !*flag {
            *flag = true;
            drop(flag);
            inner.change(&BACKGROUND_CONTEXT, |draft| draft.set("value", 2))?;
            let late = late_inner.clone();
            std::mem::forget(inner.subscribe(StateListener::sync(move |_, _, delivery| {
                late.push(delivery);
                Ok(())
            })));
        }
        Ok(())
    });
    let recorder = source_sequences.clone();
    let _b = state.member().subscribe_ops(move |_, sequence, _| {
        recorder.push(sequence);
        Ok(())
    });
    let recorder = deliveries.clone();
    let _c = state.subscribe(StateListener::sync(move |value, _, delivery| {
        if delivery.kind == DeliveryKind::Update {
            recorder.push((value.get("value").unwrap().as_i64().unwrap(), delivery.sequence));
        }
        Ok(())
    }));
    state.change(&ctx(), |draft| draft.set("value", 1)).unwrap();
    assert_eq!(source_sequences.get(), [1, 2]);
    assert_eq!(deliveries.get(), [(1, 1), (2, 2)]);
    let late: Vec<_> = late.get().iter().map(|delivery| (delivery.kind, delivery.sequence)).collect();
    assert_eq!(late, [(DeliveryKind::Hydrate, 2)]);
}

#[test]
fn isolates_listener_failures_after_committing_the_revision() {
    let state = replicated_state(json!({"value": 0}));
    let received = Recorder::new();
    let _a = state.member().subscribe_ops(|_, _, _| Err(ChordError::error("listener failed")));
    let recorder = received.clone();
    let _b = state.member().subscribe_ops(move |_, sequence, _| {
        recorder.push(sequence);
        Ok(())
    });
    let error = state.change(&ctx(), |draft| draft.set("value", 1)).unwrap_err();
    assert_eq!(error.to_string(), "listener failed");
    assert_eq!(state.value(), json!({"value": 1}));
    assert_eq!(received.get(), [1]);
}

#[test]
fn copies_assigned_values_by_value() {
    let external = json!({"value": 1});
    let state = replicated_state(json!({"left": null, "right": null}));
    state
        .change(&ctx(), |draft| {
            draft.set("left", external.clone())?;
            draft.set("right", external.clone())?;
            draft.get_mut("left").unwrap().set("value", 2)
        })
        .unwrap();
    assert_eq!(external, json!({"value": 1}));
    assert_eq!(state.value(), json!({"left": {"value": 2}, "right": {"value": 1}}));
}

#[test]
fn takes_immutable_ownership_of_replacements() {
    let state = replicated_state(json!({"left": {"value": 1}, "right": {"value": 2}}));
    let left = state.value().get("left").unwrap().clone();
    let mut replacement = json!({});
    replacement.set("left", left.clone()).unwrap();
    replacement.set("right", left).unwrap();
    state.replace(&ctx(), replacement.clone()).unwrap();
    assert!(state.value().same_ref(&replacement));
    state.change(&ctx(), |draft| draft.get_mut("left").unwrap().set("value", 9)).unwrap();
    assert_eq!(state.value(), json!({"left": {"value": 9}, "right": {"value": 1}}));
}

#[test]
fn preserves_compact_string_splice_and_permutation_operations() {
    let state = replicated_state(json!({"text": "abcdefgh", "values": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}));
    let batches = Recorder::new();
    let recorder = batches.clone();
    let _stop = state.member().subscribe_ops(move |ops, _, _| {
        recorder.push(Json::array(ops.iter().map(Op::to_json).collect()));
        Ok(())
    });
    state
        .change(&ctx(), |draft| {
            draft.set("text", "defghxyz")?;
            draft.get_mut("values").unwrap().as_array_mut().unwrap().remove(0);
            Ok(())
        })
        .unwrap();
    state
        .change(&ctx(), |draft| {
            draft.get_mut("values").unwrap().as_array_mut().unwrap().reverse();
            Ok(())
        })
        .unwrap();
    assert_eq!(
        batches.get(),
        [
            json!([["t", ["text"], 3], ["a", ["text"], "xyz"], ["p", ["values"], 0, 1, []]]),
            json!([["m", ["values"], [1, 0]]]),
        ]
    );
}

fn silent() -> ErrorReporter {
    Arc::new(|_| {})
}

#[test]
fn shares_replica_revisions_with_their_payloads() {
    let replica = ReplicatedStateReplica::new(silent());
    let root = json!({"rows": [{"value": 1}]});
    replica.hydrate(0, &[Op::Replace(root.clone())], &ctx()).unwrap();
    assert!(replica.value().unwrap().same_ref(&root));
    let inserted = json!({"value": 2});
    replica.update(1, &[Op::Splice(path!["rows"], 1, 0, vec![inserted.clone()])], &ctx()).unwrap();
    let value = replica.value().unwrap();
    assert_eq!(value.get("rows"), Some(&json!([{"value": 1}, {"value": 2}])));
    assert!(value.pointer(&path!["rows", 0usize]).unwrap().same_ref(root.pointer(&path!["rows", 0usize]).unwrap()));
    assert!(value.pointer(&path!["rows", 1usize]).unwrap().same_ref(&inserted));
    assert_eq!(root.get("rows").unwrap().as_array().unwrap().len(), 1);
}

#[test]
fn clears_a_replica_when_an_update_is_invalid() {
    let errors: Recorder<String> = Recorder::new();
    let recorder = errors.clone();
    let report: ErrorReporter = Arc::new(move |error| recorder.push(error.to_string()));
    let replica = ReplicatedStateReplica::new(report.clone());
    replica.hydrate(0, &ops(json!([["r", {"value": 0}]])), &ctx()).unwrap();
    assert!(replica.update(1, &ops(json!([["a", ["value"], "x"]])), &ctx()).is_err());
    assert!(replica.value().is_none());
    let error = replica.update(2, &ops(json!([["s", ["value"], 2]])), &ctx()).unwrap_err();
    assert!(error.to_string().contains("before hydration"));
    let malformed = ReplicatedStateReplica::new(report);
    malformed.hydrate(0, &ops(json!([["r", {"values": [1, 2]}]])), &ctx()).unwrap();
    assert!(malformed.update(1, &ops(json!([["m", ["values"], [0]]])), &ctx()).is_err());
    assert!(malformed.value().is_none());
    assert!(errors.get().is_empty());
}

#[test]
fn replaces_atomically_and_ignores_deeply_equal_replacements() {
    let state = replicated_state(json!({"value": {"nested": 1}, "retained": {"nested": 2}}));
    let previous = state.value();
    state.replace(&ctx(), json!({"value": {"nested": 1}, "retained": {"nested": 2}})).unwrap();
    assert!(state.value().same_ref(&previous));
    let mut next = state.value();
    next.set("value", json!({"nested": 2})).unwrap();
    state.replace(&ctx(), next).unwrap();
    assert_eq!(state.value(), json!({"value": {"nested": 2}, "retained": {"nested": 2}}));
    assert!(state.value().get("retained").unwrap().same_ref(previous.get("retained").unwrap()));
}
