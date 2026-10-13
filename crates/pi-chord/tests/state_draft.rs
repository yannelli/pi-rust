//! Port of upstream `test/state-draft.test.ts`. The Rust draft is a copy-on-write `&mut Json`,
//! so proxy identity, mutating sort comparators, and array holes have no Rust form.

use std::collections::BTreeMap;

use pi_chord::delta::{apply_immutable, track};
use pi_chord::{CopyJsonOptions, Json, copy_json, json, path};

fn number(value: &Json) -> f64 {
    value.as_f64().unwrap()
}

#[test]
fn copies_only_changed_branches() {
    let mut tracker = track(json!({"changed": {"count": 1, "sibling": {"value": "kept"}}, "untouched": {"value": 2}}));
    let base = tracker.value().clone();
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("changed").unwrap().set("count", 3).unwrap();
    let prepared = change.prepare().unwrap();
    assert!(!prepared.value.same_ref(&base));
    assert!(!prepared.value.get("changed").unwrap().same_ref(base.get("changed").unwrap()));
    let sibling = path!["changed", "sibling"];
    assert!(prepared.value.pointer(&sibling).unwrap().same_ref(base.pointer(&sibling).unwrap()));
    assert!(prepared.value.get("untouched").unwrap().same_ref(base.get("untouched").unwrap()));
}

#[test]
fn supports_deletion_and_native_array_mutators() {
    let mut tracker = track(json!({"optional": "remove", "values": [3, 1, 2]}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    assert_eq!(state.remove("optional"), Some(json!("remove")));
    let values = state.get_mut("values").unwrap().as_array_mut().unwrap();
    values.push(json!(4));
    assert_eq!(values.pop(), Some(json!(4)));
    values.insert(0, json!(0));
    assert_eq!(values.remove(0), json!(0));
    let removed: Vec<Json> = values.splice(1..2, [json!(5), json!(4)]).collect();
    assert_eq!(removed, [json!(1)]);
    values.sort_by(|left, right| number(left).total_cmp(&number(right)));
    values.reverse();
    values[1..3].fill(json!(9));
    let source = values[0..2].to_vec();
    values[1..3].clone_from_slice(&source);
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value, json!({"values": [5, 5, 9, 2]}));
    assert_eq!(apply_immutable(Some(&prepared.base), &prepared.ops).unwrap(), prepared.value);
}

#[test]
fn copies_inserted_draft_values_without_aliasing_their_source() {
    let mut tracker = track(json!({"values": [{"value": 1}, {"value": 2}]}));
    let mut change = tracker.begin_change();
    let values = change.state().unwrap().get_mut("values").unwrap();
    let held = values.at(0).unwrap().clone();
    values.as_array_mut().unwrap().insert(0, held);
    // Upstream writes through the held proxy, which now sits at index 1.
    values.at_mut(1).unwrap().set("value", 9).unwrap();
    let prepared = change.prepare().unwrap();
    let values = prepared.value.get("values").unwrap();
    assert_eq!(values, &json!([{"value": 1}, {"value": 9}, {"value": 2}]));
    assert!(!values.at(0).unwrap().same_ref(values.at(1).unwrap()));
}

#[test]
fn drops_writes_through_detached_child_handles() {
    let mut tracker = track(json!({"child": {"value": "removed"}, "items": [{"value": "removed"}, {"value": "kept"}]}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    let mut child = state.get("child").unwrap().clone();
    let mut shifted = state.get_mut("items").unwrap().as_array_mut().unwrap().remove(0);
    state.remove("child");
    // A Rust handle is an owned copy, so writes to it never reach the draft.
    child.set("value", "detached child").unwrap();
    shifted.set("value", "detached item").unwrap();
    assert_eq!(change.prepare().unwrap().value, json!({"items": [{"value": "kept"}]}));
}

fn insert_many(at_front: bool) {
    let mut tracker = track(json!({"values": [-1]}));
    let mut change = tracker.begin_change();
    let values = change.state().unwrap().get_mut("values").unwrap().as_array_mut().unwrap();
    let items = (0..100_000).map(Json::from);
    if at_front {
        values.splice(0..0, items);
    } else {
        values.splice(1..1, items);
    }
    let prepared = change.prepare().unwrap();
    let values = prepared.value.get("values").unwrap().as_array().unwrap();
    assert_eq!(values.len(), 100_001);
    assert_eq!(values[if at_front { 0 } else { 1 }], json!(0));
    assert_eq!(values[if at_front { 99_999 } else { 100_000 }], json!(99_999));
    assert_eq!(apply_immutable(Some(&prepared.base), &prepared.ops).unwrap(), prepared.value);
}

#[test]
fn inserts_100000_items_with_unshift() {
    insert_many(true);
}

#[test]
fn inserts_100000_items_with_splice() {
    insert_many(false);
}

#[test]
fn rejects_non_strict_json_placements_without_mutating_the_draft_or_base() {
    let mut tracker = track(json!({"number": 0, "payload": null, "values": [1, 2]}));
    let mut change = tracker.begin_change();
    // `undefined` and NaN fail at conversion, before anything reaches the draft.
    assert!(Json::from_f64(f64::NAN).is_none());
    assert!(Json::from_f64(f64::INFINITY).is_none());
    let error = copy_json(&f64::NAN, CopyJsonOptions::default()).unwrap_err();
    assert!(error.to_string().contains("strict JSON"), "{error}");
    let nested = BTreeMap::from([("nested", None::<i32>)]);
    let error = copy_json(&nested, CopyJsonOptions::default()).unwrap_err();
    assert!(error.to_string().contains("strict JSON"), "{error}");
    assert_eq!(change.peek().unwrap(), tracker.value());
    change.abort();
    assert_eq!(tracker.value(), &json!({"number": 0, "payload": null, "values": [1, 2]}));
}
