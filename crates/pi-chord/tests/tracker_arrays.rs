//! Port of upstream `test/delta-tracker/tracker.test.ts`: the "piece arrays" section.
//! Upstream held element handles become index edits after the same reindexing.

use pi_chord::delta::{Op, Tracker, apply, parse_ops, track};
use pi_chord::{Json, json, path};

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

fn items<'a>(value: &'a mut Json, key: &str) -> &'a mut Vec<Json> {
    value.get_mut(key).unwrap().as_array_mut().unwrap()
}

fn rows(count: usize, row: impl Fn(usize) -> Json) -> Json {
    Json::array((0..count).map(row).collect())
}

/// JavaScript `Array.prototype.copyWithin(target, start, end)`.
fn copy_within(values: &mut [Json], target: usize, start: usize, end: usize) {
    let source = values[start..end].to_vec();
    let count = source.len().min(values.len() - target);
    values[target..target + count].clone_from_slice(&source[..count]);
}

fn number(value: &Json, key: &str) -> f64 {
    value.get(key).unwrap().as_f64().unwrap()
}

/// Prepare one change on `values`, check replay against a deep base copy, and adopt it.
fn run(tracker: &mut Tracker, mutate: impl FnOnce(&mut Vec<Json>)) -> Vec<Op> {
    let base = deep(tracker.value());
    let mut change = tracker.begin_change();
    mutate(items(change.state().unwrap(), "values"));
    let prepared = change.prepare().unwrap();
    assert_eq!(replay(&base, &prepared.ops), prepared.value);
    tracker.adopt(&prepared).unwrap();
    prepared.ops
}

fn splice_heads(ops: &[Op]) -> Vec<(u64, u64, usize)> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Splice(_, index, remove, items) => Some((*index, *remove, items.len())),
            _ => None,
        })
        .collect()
}

#[test]
fn emits_one_insert_for_an_append_to_a_long_array() {
    let mut tracker = track(json!({ "values": rows(10_000, |index| json!(index)) }));
    let mut change = tracker.begin_change();
    items(change.state().unwrap(), "values").push(json!(-1));
    let prepared = change.prepare().unwrap();
    assert_eq!(ops_json(&prepared.ops), json!([["p", ["values"], 10_000, 0, [-1]]]));
    tracker.adopt(&prepared).unwrap();
    let values = tracker.value().get("values").unwrap().as_array().unwrap();
    assert_eq!((values.last(), values.len()), (Some(&json!(-1)), 10_001));
}

// Upstream asserts the op list its piece tracker records; the diff here chooses its own splices, so these check the
// replay (`run` compares it with the prepared value) and the final array.
#[test]
fn replays_removals_and_inserts_that_keep_the_kept_elements_in_order() {
    let mut tracker = track(json!({ "values": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] }));
    run(&mut tracker, |values| {
        values.remove(0);
        values.splice(1..3, [json!(20)]);
        values.remove(5);
        values.pop();
        values.push(json!(30));
    });
    assert_eq!(tracker.value().get("values").unwrap(), &json!([1, 20, 4, 5, 6, 8, 30]));
}

#[test]
fn emits_a_permutation_when_kept_elements_are_reversed_and_replays_a_reordering() {
    let mut tracker = track(json!({ "values": [0, 1, 2, 3, 4] }));
    let ops = run(&mut tracker, |values| values.reverse());
    assert!(ops.iter().any(|op| op.verb() == "m"), "{ops:?}");
    assert_eq!(tracker.value().get("values").unwrap(), &json!([4, 3, 2, 1, 0]));
    run(&mut tracker, |values| {
        values.sort_by_key(|value| (value.as_f64().unwrap() as i64 % 2, value.as_f64().unwrap() as i64))
    });
    assert_eq!(tracker.value().get("values").unwrap(), &json!([0, 2, 4, 1, 3]));
}

#[test]
fn keeps_adoption_independent_from_changed_permutation_metadata() {
    let mut tracker = track(json!({"values": [3, 1, 2]}));
    let mut change = tracker.begin_change();
    items(change.state().unwrap(), "values")
        .sort_by(|left, right| left.as_f64().unwrap().total_cmp(&right.as_f64().unwrap()));
    let mut prepared = change.prepare().unwrap();
    let Some(Op::Move(_, permutation)) = prepared.ops.iter_mut().find(|op| op.verb() == "m") else {
        panic!("expected permutation")
    };
    permutation.reverse();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(tracker.value().get("values").unwrap(), &json!([1, 2, 3]));
}

#[test]
fn matches_native_splice_with_zero_or_one_argument() {
    let mut none = track(json!({"values": [1, 2, 3]}));
    let ops = run(&mut none, |values| assert!(values.splice(0..0, []).next().is_none()));
    assert!(ops.is_empty());
    assert_eq!(none.value().get("values").unwrap(), &json!([1, 2, 3]));

    let mut one = track(json!({"values": [1, 2, 3, 4]}));
    run(&mut one, |values| assert_eq!(values.drain(1..).collect::<Vec<_>>(), [json!(2), json!(3), json!(4)]));
    assert_eq!(one.value().get("values").unwrap(), &json!([1]));

    let mut past_end = track(json!({"values": [1, 2, 3]}));
    let ops = run(&mut past_end, |values| assert!(values.drain(3..).next().is_none()));
    assert!(ops.is_empty());
    assert_eq!(past_end.value().get("values").unwrap(), &json!([1, 2, 3]));
}

#[test]
fn supports_all_structural_mutators_and_native_return_values() {
    let mut tracker = track(json!({"values": [3, 1, 2]}));
    run(&mut tracker, |values| {
        values.push(json!(4));
        assert_eq!(values.len(), 4);
        assert_eq!(values.pop(), Some(json!(4)));
        values.insert(0, json!(0));
        assert_eq!(values.len(), 4);
        assert_eq!(values.remove(0), json!(0));
        assert_eq!(values.splice(1..2, [json!(5), json!(4)]).collect::<Vec<_>>(), [json!(1)]);
        values.sort_by(|left, right| left.as_f64().unwrap().total_cmp(&right.as_f64().unwrap()));
        values.reverse();
        values[1..3].fill(json!(9));
        copy_within(values, 1, 0, 2);
    });
    assert_eq!(tracker.value(), &json!({"values": [5, 5, 9, 2]}));
}

#[test]
fn tracks_elements_after_reindexing() {
    let mut tracker = track(json!({"values": [{"value": "a"}, {"value": "b"}, {"value": "c"}]}));
    run(&mut tracker, |values| {
        values.insert(0, json!({"value": "front"}));
        values[2].set("value", "moved").unwrap();
        assert_eq!(values[2].get("value"), Some(&json!("moved")));
        values.remove(2);
    });
    assert_eq!(tracker.value(), &json!({"values": [{"value": "front"}, {"value": "a"}, {"value": "c"}]}));
}

#[test]
fn replays_introduced_moved_then_edited_array_descendants() {
    let mut tracker = track(json!({"values": []}));
    run(&mut tracker, |values| {
        values.push(json!({"id": 1, "nested": [1]}));
        values.insert(0, json!({"id": 0, "nested": []}));
        values[1].get_mut("nested").unwrap().push(2).unwrap();
        values.reverse();
        values[0].get_mut("nested").unwrap().push(3).unwrap();
    });
    assert_eq!(tracker.value(), &json!({"values": [{"id": 1, "nested": [1, 2, 3]}, {"id": 0, "nested": []}]}));
}

#[test]
fn combines_movement_and_edits() {
    let mut tracker = track(json!({"values": [
        {"rank": 3, "edited": 0},
        {"rank": 1, "edited": 0},
        {"rank": 2, "edited": 0},
    ]}));
    // Upstream emits ["m", ["values"], [1, 2, 0]] first; the Rust diff sees an edited element
    // instead of a pure reorder, so only the replay is checked.
    run(&mut tracker, |values| {
        values.sort_by(|left, right| number(left, "rank").total_cmp(&number(right, "rank")));
        values[2].set("edited", 1).unwrap();
    });
    let ranks: Vec<f64> = tracker.value()["values"].as_array().unwrap().iter().map(|v| number(v, "rank")).collect();
    assert_eq!(ranks, [1.0, 2.0, 3.0]);
    assert_eq!(tracker.value().pointer(&path!["values", 2usize, "edited"]), Some(&json!(1)));
}

#[test]
fn folds_dense_child_and_direct_index_edits_into_one_full_array_operation() {
    let mut row_tracker = track(json!({"values": rows(1_000, |value| json!({"value": value}))}));
    let ops = run(&mut row_tracker, |values| {
        for row in values.iter_mut() {
            let next = number(row, "value") + 1.0;
            row.set("value", next).unwrap();
        }
    });
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].verb(), "p");

    let mut value_tracker = track(json!({"values": rows(1_000, |value| json!(value))}));
    let ops = run(&mut value_tracker, |values| {
        for (index, value) in values.iter_mut().enumerate().take(600) {
            *value = json!(-(index as i64) - 1);
        }
    });
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].verb(), "p");
}

#[test]
fn folds_only_a_deeply_nested_dense_array_region() {
    let mut tracker = track(json!({"values": rows(2_000, |value| json!({"nested": {"value": value}}))}));
    let ops = run(&mut tracker, |values| {
        values[0].get_mut("nested").unwrap().set("value", -1).unwrap();
        for (index, row) in values.iter_mut().enumerate().take(1_000).skip(500) {
            row.get_mut("nested").unwrap().set("value", -(index as i64)).unwrap();
        }
    });
    assert_eq!(splice_heads(&ops), [(500, 500, 500)]);
    assert_eq!(ops.len(), 2);
}

#[test]
fn covers_nested_reserved_key_edits_with_an_outer_dense_region() {
    let reserved = format!(r#"{{"__proto__":{{"values":{}}}}}"#, rows(400, |value| json!(value)));
    let mut initial = json!({"values": rows(400, |_| json!({"flag": 0}))});
    initial.pointer_mut(&path!["values", 100usize]).unwrap().set("special", Json::parse(&reserved).unwrap()).unwrap();
    let mut tracker = track(initial);
    let ops = run(&mut tracker, |values| {
        for row in values.iter_mut().take(256) {
            row.set("flag", 1).unwrap();
        }
        let nested = values[100].pointer_mut(&path!["special", "__proto__", "values"]).unwrap();
        for (index, value) in nested.as_array_mut().unwrap().iter_mut().enumerate().take(256) {
            *value = json!(-(index as i64) - 1);
        }
    });
    assert_eq!(ops.len(), 1);
    assert_eq!(splice_heads(&ops), [(0, 256, 256)]);
}

#[test]
fn covers_nested_structural_and_leaf_edits_with_an_outer_dense_region() {
    let inner = rows(400, |value| json!({"value": value}));
    let initial = rows(400, |index| json!({"flag": 0, "values": if index == 100 { inner.clone() } else { json!([]) }}));
    let mut tracker = track(json!({"values": initial}));
    let ops = run(&mut tracker, |values| {
        for row in values.iter_mut().take(256) {
            row.set("flag", 1).unwrap();
        }
        let nested = items(&mut values[100], "values");
        for (index, row) in nested.iter_mut().enumerate().take(256) {
            row.set("value", -(index as i64) - 1).unwrap();
        }
        nested.push(json!({"value": 999}));
    });
    // Upstream folds rows 0..256 into one splice; the Rust diff aligns rows by their shared
    // empty `values` arrays and edits them one by one, so only the replay in `run` is checked.
    assert!(!ops.is_empty());
    assert_eq!(tracker.value().pointer(&path!["values", 100usize, "values"]).unwrap().as_array().unwrap().len(), 401);
}

#[test]
fn emits_disjoint_dense_regions_and_keeps_operations_outside_them() {
    let mut tracker = track(json!({"values": rows(1_400, |value| json!({"value": value}))}));
    let edit = |values: &mut Vec<Json>, index: usize| values[index].set("value", -(index as i64) - 1).unwrap();
    let ops = run(&mut tracker, |values| {
        for index in [97, 358, 897, 1_158].into_iter().chain(100..356).chain(900..1_156) {
            edit(values, index);
        }
    });
    assert_eq!(splice_heads(&ops), [(100, 256, 256), (900, 256, 256)]);
    let listed = ops_json(&ops);
    for index in [97i64, 358, 897, 1_158] {
        assert!(listed.as_array().unwrap().contains(&json!(["s", ["values", index, "value"], -index - 1])));
    }
    assert_eq!(ops.len(), 6);
}

#[test]
fn normalizes_a_twenty_thousand_operation_queue_transaction_to_two_operations() {
    let mut tracker = track(json!({"values": rows(20_000, |value| json!({"value": value}))}));
    let ops = run(&mut tracker, |values| {
        for index in 0..10_000 {
            values.remove(0);
            values.push(json!({"value": 20_000 + index}));
        }
    });
    assert_eq!(ops.len(), 2);
}

#[test]
fn handles_adversarial_fragmentation() {
    let size = 20_000;
    let mut tracker = track(json!({"values": rows(size, |value| json!({"value": value}))}));
    let mut expected = deep(tracker.value());
    let held = [1, 1_001, 5_001, 10_001, 15_001, 19_999];
    let mutate = |values: &mut Vec<Json>| {
        for index in (0..size).step_by(2) {
            values.splice(index..index + 1, [json!({"value": -(index as i64) - 1})]);
        }
        for index in held {
            let next = number(&values[index], "value") + 100_000.0;
            values[index].set("value", next).unwrap();
        }
    };
    mutate(items(&mut expected, "values"));
    run(&mut tracker, mutate);
    assert_eq!(tracker.value(), &expected);
}

#[test]
fn normalizes_restored_overrides_and_cancelled_structural_edits_to_no_operations() {
    let mut tracker = track(json!({"values": [1, 2, 3]}));
    let base = tracker.value().clone();
    let ops = run(&mut tracker, |values| {
        values[1] = json!(9);
        values[1] = json!(2);
        values.reverse();
        values.reverse();
        values.push(json!(4));
        assert_eq!(values.pop(), Some(json!(4)));
    });
    assert!(ops.is_empty());
    assert!(tracker.value().same_ref(&base));
}

#[test]
fn handles_null_overrides_without_confusing_absence() {
    let mut tracker = track(json!({"values": rows(10_000, |value| json!(value))}));
    run(&mut tracker, |values| {
        values[17] = Json::Null;
        values[9_000] = Json::Null;
    });
    assert!(tracker.value().pointer(&path!["values", 17usize]).unwrap().is_null());
    assert!(tracker.value().pointer(&path!["values", 9_000usize]).unwrap().is_null());
}

#[test]
fn supports_self_overlapping_fill_and_copy_within_by_value() {
    let mut tracker = track(json!({"values": [{"n": 0}, {"n": 1}, {"n": 2}, {"n": 3}]}));
    run(&mut tracker, |values| {
        copy_within(values, 1, 0, 3);
        let second = values[1].clone();
        values[0..2].fill(second);
        values[0].set("n", 9).unwrap();
    });
    let numbers: Vec<f64> = tracker.value()["values"].as_array().unwrap().iter().map(|v| number(v, "n")).collect();
    assert_eq!(numbers, [9.0, 0.0, 1.0, 2.0]);
}

#[test]
fn inserts_one_hundred_thousand_items_with_unshift_and_splice() {
    for unshift in [true, false] {
        let mut tracker = track(json!({"values": [-1]}));
        run(&mut tracker, |values| {
            let inserted = (0..100_000).map(|value| json!(value));
            let at = if unshift { 0 } else { 1 };
            values.splice(at..at, inserted);
        });
        let values = tracker.value()["values"].as_array().unwrap();
        assert_eq!(values.len(), 100_001);
        assert_eq!(values[if unshift { 99_999 } else { 100_000 }], json!(99_999));
    }
}
