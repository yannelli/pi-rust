//! Ports of upstream `test/delta-clone.test.ts` and `test/state-value.test.ts` (tracker ownership).
//! `Object.isFrozen`, null prototypes, and accessor traversal have no Rust form: `Json` is
//! plain, strict, and immutable behind `Arc` by construction.

use pi_chord::delta::{apply, track};
use pi_chord::{Json, json, path};

#[test]
fn takes_immutable_ownership_of_the_imported_revision_in_o1() {
    let input = json!({
        "point": {"x": 3, "y": 7, "pressure": 0.1},
        "rows": [{"values": [0, false, null, "text", {"n": 1}]}],
    });
    let tracker = track(input.clone());
    assert!(tracker.value().same_ref(&input));
    assert!(tracker.value().get("point").unwrap().same_ref(input.get("point").unwrap()));
    let leaf = path!["rows", 0usize, "values", 4usize];
    assert!(tracker.value().pointer(&leaf).unwrap().same_ref(input.pointer(&leaf).unwrap()));
}

#[test]
fn takes_ownership_of_an_alias_free_root_with_distinct_siblings() {
    let input = json!({"left": {"nested": [{"n": 1}]}, "right": {"nested": [{"n": 1}]}});
    let tracker = track(input.clone());
    let value = tracker.value();
    assert!(value.same_ref(&input));
    assert!(!value.get("left").unwrap().same_ref(value.get("right").unwrap()));
    let left = value.pointer(&path!["left", "nested", 0usize]).unwrap();
    assert!(!left.same_ref(value.pointer(&path!["right", "nested", 0usize]).unwrap()));
}

#[test]
fn copies_assigned_and_inserted_values_immediately() {
    let mut tracker = track(json!({"rows": []}));
    let mut assigned = json!({"nested": {"value": 1}});
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("rows").unwrap().push(assigned.clone()).unwrap();
    assigned.get_mut("nested").unwrap().set("value", 9).unwrap();
    let nested_value = path!["rows", 0usize, "nested", "value"];
    assert_eq!(change.peek().unwrap().pointer(&nested_value), Some(&json!(1)));
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value.pointer(&nested_value), Some(&json!(1)));
    let replica = apply(Some(prepared.base.clone()), &prepared.ops).unwrap();
    assert_eq!(replica, prepared.value);
    assert_eq!(assigned, json!({"nested": {"value": 9}}));
}

#[test]
fn commits_transaction_copies_while_sharing_unchanged_branches() {
    let mut tracker = track(json!({"changed": {"value": 1}, "retained": {"value": 2}}));
    let base = tracker.value().clone();
    let snapshot = Json::parse(&base.to_string()).unwrap();
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("changed").unwrap().set("value", 3).unwrap();
    let prepared = change.prepare().unwrap();
    assert!(!prepared.value.get("changed").unwrap().same_ref(base.get("changed").unwrap()));
    assert!(prepared.value.get("retained").unwrap().same_ref(base.get("retained").unwrap()));
    tracker.adopt(&prepared).unwrap();
    assert_eq!(base, snapshot);
    assert!(tracker.value().same_ref(&prepared.value));
}

#[test]
fn makes_repeated_placements_independent() {
    let mut tracker = track(json!({"values": []}));
    let shared = json!({"value": 1});
    let mut change = tracker.begin_change();
    let values = change.state().unwrap().get_mut("values").unwrap();
    values.push(shared.clone()).unwrap();
    values.push(shared).unwrap();
    let prepared = change.prepare().unwrap();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(prepared.value, json!({"values": [{"value": 1}, {"value": 1}]}));
    // Rust placements may share one `Arc`; independence means an edit to one never reaches the other.
    let mut edit = tracker.begin_change();
    edit.state().unwrap().pointer_mut(&path!["values", 0usize]).unwrap().set("value", 2).unwrap();
    let edited = edit.prepare().unwrap();
    assert_eq!(edited.value, json!({"values": [{"value": 2}, {"value": 1}]}));
    assert_eq!(prepared.value, json!({"values": [{"value": 1}, {"value": 1}]}));
    tracker.adopt(&edited).unwrap();
}
