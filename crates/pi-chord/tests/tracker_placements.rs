//! Port of upstream `test/delta-tracker/tracker.test.ts`: by-value placements and storage cloning.
//! Rust values are immutable and shared, so upstream checks that two placements are distinct
//! objects have no meaning here; only their values are compared.

use pi_chord::delta::{Op, Seg, apply, parse_ops, track};
use pi_chord::{Json, JsonMap, json, path};

fn deep(value: &Json) -> Json {
    Json::parse(&value.to_string()).unwrap()
}

fn ops_json(ops: &[Op]) -> Json {
    Json::array(ops.iter().map(Op::to_json).collect())
}

fn replay(base: &Json, ops: &[Op]) -> Json {
    let detached = parse_ops(&Json::parse(&ops_json(ops).to_string()).unwrap()).unwrap();
    apply(Some(deep(base)), &detached).unwrap()
}

fn keys(value: &Json) -> Vec<String> {
    value.as_object().unwrap().keys().map(|key| key.to_string()).collect()
}

fn items<'a>(value: &'a mut Json, key: &str) -> &'a mut Vec<Json> {
    value.get_mut(key).unwrap().as_array_mut().unwrap()
}

fn put(root: &mut Json, path: &[Seg], value: impl Into<Json>) {
    *root.pointer_mut(path).unwrap() = value.into();
}

/// JavaScript `Array.prototype.copyWithin(target, start, end)`.
fn copy_within(values: &mut [Json], target: usize, start: usize, end: usize) {
    let source = values[start..end].to_vec();
    let count = source.len().min(values.len() - target);
    values[target..target + count].clone_from_slice(&source[..count]);
}

#[test]
fn places_property_index_push_unshift_splice_fill_and_copy_within_by_value() {
    let mut tracker = track(json!({"property": null, "values": [{"value": 0}, {"value": 1}, {"value": 2}]}));
    let base = deep(tracker.value());
    let mut external = json!({"value": 5});
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    state.set("property", external.clone()).unwrap();
    let values = items(state, "values");
    values[0] = external.clone();
    values.push(external.clone());
    values.insert(0, external.clone());
    values.insert(2, external.clone());
    external.set("value", 99).unwrap();
    let first = values[0].clone();
    values[1..3].fill(first);
    copy_within(values, 3, 0, 2);
    let prepared = change.prepare().unwrap();
    let candidate = deep(&prepared.value);
    assert_eq!(candidate.get("property").unwrap(), &json!({"value": 5}));
    let placed = candidate.get("values").unwrap().as_array().unwrap();
    assert!(placed[..5].iter().all(|value| value == &json!({"value": 5})));
    assert_eq!(replay(&base, &prepared.ops), prepared.value);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn accepts_strict_placements_and_rejects_non_finite_numbers() {
    // Non-strict inputs such as undefined, NaN, and cycles have no `Json` form; only numbers are checked.
    assert!(Json::from_f64(f64::NAN).is_none());
    assert!(Json::from_f64(f64::INFINITY).is_none());
    let initial = json!({"optional": "remove", "payload": null, "number": 0, "values": [1, 2]});
    let mut tracker = track(initial.clone());
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    assert_eq!(state, &initial);
    state.remove("optional");
    let mut dictionary = JsonMap::new();
    dictionary.insert("valid", json!(true));
    state.set("payload", Json::object(dictionary)).unwrap();
    items(state, "values").push(json!({"valid": true}));
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value, json!({"payload": {"valid": true}, "number": 0, "values": [1, 2, {"valid": true}]}));
    assert_eq!(replay(&initial, &prepared.ops), prepared.value);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn expands_repeated_source_aliases_into_independent_placements() {
    let mut tracker = track(json!({"left": null, "right": null, "rows": []}));
    let shared = json!({"nested": {"value": 1}});
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    state.set("left", shared.clone()).unwrap();
    state.set("right", shared.clone()).unwrap();
    items(state, "rows").extend([shared.clone(), shared.clone()]);
    put(state, &path!["left", "nested", "value"], 9);
    put(state, &path!["rows", 0usize, "nested", "value"], 8);
    let prepared = change.prepare().unwrap();
    assert_eq!(
        prepared.value,
        json!({
            "left": {"nested": {"value": 9}},
            "right": {"nested": {"value": 1}},
            "rows": [{"nested": {"value": 8}}, {"nested": {"value": 1}}],
        })
    );
    assert_eq!(shared, json!({"nested": {"value": 1}}));
    assert_eq!(replay(tracker.value(), &prepared.ops), prepared.value);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn copies_draft_sourced_placements_within_and_across_revisions() {
    let mut tracker = track(json!({
        "a": {"child": {"value": 1}},
        "b": null,
        "rows": [{"child": {"value": 1}}, {"child": {"value": 2}}],
    }));
    let mut first = tracker.begin_change();
    let state = first.state().unwrap();
    let a = state.get("a").unwrap().clone();
    state.set("b", a).unwrap();
    put(state, &path!["b", "child", "value"], 3);
    let row = state.pointer(&path!["rows", 0usize]).unwrap().clone();
    put(state, &path!["rows", 1usize], row);
    let first_prepared = first.prepare().unwrap();
    assert_eq!(first_prepared.value.pointer(&path!["a", "child", "value"]), Some(&json!(1)));
    assert_eq!(first_prepared.value.pointer(&path!["b", "child", "value"]), Some(&json!(3)));
    assert_eq!(first_prepared.value.get("rows").unwrap(), &json!([{"child": {"value": 1}}, {"child": {"value": 1}}]));
    tracker.adopt(&first_prepared).unwrap();

    let base = deep(tracker.value());
    let mut second = tracker.begin_change();
    put(second.state().unwrap(), &path!["rows", 1usize, "child", "value"], 4);
    let second_prepared = second.prepare().unwrap();
    let rows = second_prepared.value.get("rows").unwrap().as_array().unwrap();
    let values: Vec<&Json> = rows.iter().map(|row| row.pointer(&path!["child", "value"]).unwrap()).collect();
    assert_eq!(values, [&json!(1), &json!(4)]);
    assert_eq!(replay(&base, &second_prepared.ops), second_prepared.value);
    tracker.adopt(&second_prepared).unwrap();
}

#[test]
fn distinguishes_committed_values_from_draft_values() {
    let mut tracker = track(json!({"source": {"value": 1}, "rawCopy": null, "draftCopy": null}));
    let raw = tracker.value().get("source").unwrap().clone();
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    put(state, &path!["source", "value"], 2);
    state.set("rawCopy", raw).unwrap();
    let source = state.get("source").unwrap().clone();
    state.set("draftCopy", source).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value, json!({"source": {"value": 2}, "rawCopy": {"value": 1}, "draftCopy": {"value": 2}}));
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn folds_edits_to_introduced_subtrees_into_placement_payloads() {
    let mut tracker = track(json!({"nested": null, "rows": []}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    state.set("nested", json!({"rows": [{"value": 1}]})).unwrap();
    put(state, &path!["nested", "rows", 0usize, "value"], 2);
    state.pointer_mut(&path!["nested", "rows"]).unwrap().push(json!({"value": 3})).unwrap();
    items(state, "rows").push(json!({"values": [1]}));
    state.pointer_mut(&path!["rows", 0usize, "values"]).unwrap().push(2).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(
        ops_json(&prepared.ops),
        json!([
            ["s", ["nested"], {"rows": [{"value": 2}, {"value": 3}]}],
            ["p", ["rows"], 0, 0, [{"values": [1, 2]}]],
        ])
    );
    assert_eq!(replay(tracker.value(), &prepared.ops), prepared.value);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn folds_pathological_operation_counts_to_a_root_replacement() {
    let wide: JsonMap = (0..5_000).map(|index| (format!("field{index}"), json!(0))).collect();
    let mut wide_tracker = track(Json::object(wide));
    let wide_base = deep(wide_tracker.value());
    let mut wide_change = wide_tracker.begin_change();
    let state = wide_change.state().unwrap();
    for index in 0..5_000 {
        state.set(format!("field{index}"), 1).unwrap();
    }
    let wide_prepared = wide_change.prepare().unwrap();
    assert_eq!(wide_prepared.ops.len(), 1);
    assert!(wide_prepared.ops[0].is_replace());
    assert_eq!(replay(&wide_base, &wide_prepared.ops), wide_prepared.value);
    wide_tracker.adopt(&wide_prepared).unwrap();

    let rows: Vec<Json> = (0..15_000).map(|value| json!({"value": value})).collect();
    let mut sparse_tracker = track(json!({"rows": rows}));
    let sparse_base = deep(sparse_tracker.value());
    let mut sparse_change = sparse_tracker.begin_change();
    let state = sparse_change.state().unwrap();
    for index in (0..15_000usize).step_by(3) {
        put(state, &path!["rows", index, "value"], -(index as i64) - 1);
    }
    let sparse_prepared = sparse_change.prepare().unwrap();
    assert_eq!(sparse_prepared.ops.len(), 1);
    assert!(sparse_prepared.ops[0].is_replace());
    assert_eq!(replay(&sparse_base, &sparse_prepared.ops), sparse_prepared.value);
    sparse_tracker.adopt(&sparse_prepared).unwrap();
}

#[test]
fn enumerates_wide_objects_without_duplicate_keys() {
    let mut tracker = track(json!({"values": {}}));
    let mut change = tracker.begin_change();
    let values = change.state().unwrap().get_mut("values").unwrap();
    for index in 0..20_000 {
        values.set(format!("field{index}"), index).unwrap();
    }
    let listed = keys(values);
    assert_eq!(listed.len(), 20_000);
    assert_eq!(listed[0], "field0");
    assert_eq!(listed.last().unwrap(), "field19999");
    change.abort();
}

#[test]
fn does_not_dirty_read_only_traversals() {
    let mut tracker = track(json!({"nested": {"rows": [{"value": 1}]}}));
    let mut change = tracker.begin_change();
    let leaf = path!["nested", "rows", 0usize, "value"];
    assert_eq!(change.peek().unwrap().pointer(&leaf), Some(&json!(1)));
    // A mutable walk copies containers on write; the diff still finds no change.
    assert_eq!(change.state().unwrap().pointer_mut(&leaf), Some(&mut json!(1)));
    let prepared = change.prepare().unwrap();
    assert!(prepared.ops.is_empty());
    assert!(prepared.value.same_ref(&prepared.base));
    tracker.adopt(&prepared).unwrap();
}

/// A MemoryStorage-style recursive copy that rebuilds every container.
fn storage_clone(value: &Json) -> Json {
    match value {
        Json::Array(values) => Json::array(values.iter().map(storage_clone).collect()),
        Json::Object(map) => Json::object(map.iter().map(|(key, value)| (key.clone(), storage_clone(value))).collect()),
        other => other.clone(),
    }
}

#[test]
fn supports_a_storage_style_recursive_clone_of_the_prepared_value() {
    let mut tracker = track(json!({"rows": [{"value": 1}]}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    put(state, &path!["rows", 0usize, "value"], 2);
    items(state, "rows").push(json!({"value": 3}));
    let prepared = change.prepare().unwrap();
    let copied = storage_clone(&prepared.value);
    assert_eq!(copied, json!({"rows": [{"value": 2}, {"value": 3}]}));
    assert!(!copied.same_ref(&prepared.value));
    tracker.adopt(&prepared).unwrap();
}
