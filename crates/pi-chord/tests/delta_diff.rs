//! Port of upstream `test/delta-diff.test.ts`.

use pi_chord::delta::{
    Op, apply_immutable, assert_valid_op, assert_valid_wire_op, decoder, diff_revisions, encoder, parse_ops,
};
use pi_chord::{Json, JsonMap, json};

fn ops_json(ops: &[Op]) -> Json {
    Json::array(ops.iter().map(Op::to_json).collect())
}

fn expect_diff(before: Json, after: Json, expected: Json) {
    let operations = diff_revisions(&before, &after);
    assert_eq!(ops_json(&operations), expected);
    assert_eq!(apply_immutable(Some(&before), &operations).unwrap(), after);
}

fn ids(names: &[&str]) -> Vec<Json> {
    names.iter().map(|id| json!({ "id": id })).collect()
}

fn values(items: Vec<Json>) -> Json {
    let mut map = JsonMap::new();
    map.insert("values", Json::array(items));
    Json::object(map)
}

#[test]
fn emits_sets_and_deletes() {
    expect_diff(
        json!({"keep": 1, "change": 1, "remove": true}),
        json!({"keep": 1, "change": 2, "add": 3}),
        json!([["s", ["change"], 2], ["s", ["add"], 3], ["d", ["remove"]]]),
    );
}

#[test]
fn emits_string_append_and_front_truncation() {
    expect_diff(json!({"text": "hello"}), json!({"text": "hello world"}), json!([["a", ["text"], " world"]]));
    expect_diff(json!({"text": "hello world"}), json!({"text": "world"}), json!([["t", ["text"], 6]]));
    expect_diff(
        json!({"text": "abcdefgh"}),
        json!({"text": "defghxyz"}),
        json!([["t", ["text"], 3], ["a", ["text"], "xyz"]]),
    );
}

#[test]
fn counts_truncation_in_utf16_units() {
    expect_diff(
        json!({"text": "\u{1F600}ab"}),
        json!({"text": "abc"}),
        json!([["t", ["text"], 2], ["a", ["text"], "c"]]),
    );
}

#[test]
fn represents_array_insertion_removal_and_shift_with_splices() {
    let [a, b, c] = <[Json; 3]>::try_from(ids(&["a", "b", "c"])).unwrap();
    expect_diff(
        values(vec![a.clone(), b.clone()]),
        values(vec![a.clone(), c.clone(), b.clone()]),
        Json::array(vec![json!(["p", ["values"], 1, 0]).pushed(Json::array(vec![c.clone()]))]),
    );
    expect_diff(values(vec![a, b.clone(), c.clone()]), values(vec![b, c]), json!([["p", ["values"], 0, 1, []]]));
}

trait Pushed {
    fn pushed(self, value: Json) -> Json;
}

impl Pushed for Json {
    fn pushed(mut self, value: Json) -> Json {
        self.push(value).unwrap();
        self
    }
}

#[test]
fn collapses_a_same_length_queue_update_to_two_splices() {
    let [a, b, c, d] = <[Json; 4]>::try_from(ids(&["a", "b", "c", "d"])).unwrap();
    expect_diff(
        values(vec![a, b.clone(), c.clone()]),
        values(vec![b, c, d]),
        json!([["p", ["values"], 0, 1, []], ["p", ["values"], 2, 0, [{"id": "d"}]]]),
    );
}

#[test]
fn emits_a_permutation_for_a_pure_reorder() {
    let [a, b, c] = <[Json; 3]>::try_from(ids(&["a", "b", "c"])).unwrap();
    expect_diff(
        values(vec![a.clone(), b.clone(), c.clone()]),
        values(vec![c, a, b]),
        json!([["m", ["values"], [2, 0, 1]]]),
    );
}

#[test]
fn normalizes_reordered_distinct_deeply_equal_objects_to_a_noop() {
    let first = json!({"nested": {"value": 1}});
    let second = json!({"nested": {"value": 1}});
    assert!(!first.same_ref(&second));
    assert!(diff_revisions(&values(vec![first.clone(), second.clone()]), &values(vec![second, first])).is_empty());
}

#[test]
fn validates_and_encodes_permutations() {
    let operations = parse_ops(&json!([["m", ["values"], [2, 0, 1]], ["m", ["values"], [1, 2, 0]]])).unwrap();
    let wire = encoder().encode(&operations);
    let wire_json = Json::array(wire.iter().map(|op| op.to_json()).collect());
    assert_eq!(wire_json, json!([["m", ["values"], [2, 0, 1]], ["m", [1, 2, 0]]]));
    for op in wire_json.as_array().unwrap() {
        assert_valid_wire_op(op).unwrap();
    }
    assert_eq!(decoder().decode(&wire).unwrap(), operations);
    let error = assert_valid_op(&json!(["m", ["values"], [0, 0]])).unwrap_err();
    assert!(error.to_string().contains("bijection"));
}

#[test]
fn emits_nothing_for_deeply_equal_reconstructed_values() {
    assert!(diff_revisions(&json!({"value": {"nested": [1, 2]}}), &json!({"value": {"nested": [1, 2]}})).is_empty());
    assert!(
        diff_revisions(&json!({"values": [{"id": 1}, {"id": 2}]}), &json!({"values": [{"id": 1}, {"id": 2}]}))
            .is_empty()
    );
    assert!(diff_revisions(&json!({"values": [true, true, true]}), &json!({"values": [true, true, true]})).is_empty());
}

#[test]
fn keeps_a_leaf_edit_inside_a_reconstructed_array_narrow() {
    expect_diff(
        json!({"values": [{"id": 1, "label": "one"}, {"id": 2, "label": "two"}]}),
        json!({"values": [{"id": 1, "label": "one"}, {"id": 2, "label": "changed"}]}),
        json!([["s", ["values", 1, "label"], "changed"]]),
    );
}

#[test]
fn emits_payload_free_splices_for_scattered_removals() {
    let [a, b, c, d, e] = <[Json; 5]>::try_from(ids(&["a", "b", "c", "d", "e"])).unwrap();
    expect_diff(
        values(vec![a.clone(), b, c.clone(), d, e.clone()]),
        values(vec![a, c, e]),
        json!([["p", ["values"], 1, 1, []], ["p", ["values"], 2, 1, []]]),
    );
}

#[test]
fn encodes_removals_canonically() {
    let cases = [
        (json!([1, 2, 3, 4]), json!([2, 3, 4]), json!([["p", ["values"], 0, 1, []]])),
        (json!([1, 2, 3, 4]), json!([1, 2, 3]), json!([["p", ["values"], 3, 1, []]])),
        (json!([1, 2, 3, 4]), json!([1, 3, 4]), json!([["p", ["values"], 1, 1, []]])),
        (json!([1, 2, 3, 4]), json!([]), json!([["p", ["values"], 0, 4, []]])),
        (json!([1, 2, 3, 4]), json!([1, 2, 3, 4]), json!([])),
    ];
    for (before, after, expected) in cases {
        expect_diff(values(before.as_array().unwrap().clone()), values(after.as_array().unwrap().clone()), expected);
    }
}

#[test]
fn does_not_field_diff_unrelated_shifted_objects_with_common_fields() {
    let rows = |range: [i32; 3]| range.iter().map(|value| json!({"type": "row", "value": value})).collect();
    expect_diff(
        values(rows([1, 2, 3])),
        values(rows([2, 3, 4])),
        json!([["p", ["values"], 0, 1, []], ["p", ["values"], 2, 0, [{"type": "row", "value": 4}]]]),
    );
}

#[test]
fn does_not_treat_coincidental_id_fields_as_identity() {
    let left = json!({"value": "left"});
    let right = json!({"value": "right"});
    let before = vec![
        left.clone(),
        json!({"id": 1, "key": "a", "value": "first"}),
        json!({"id": 2, "key": "b", "value": "second"}),
        right.clone(),
    ];
    let replacements = vec![
        json!({"id": 2, "key": "b", "value": "edited-second"}),
        json!({"id": 1, "key": "a", "value": "edited-first"}),
    ];
    let after = vec![left, replacements[0].clone(), replacements[1].clone(), right];
    expect_diff(values(before), values(after), json!(["p", ["values"], 1, 2]).pushed(Json::array(replacements)).wrap());
}

trait Wrap {
    fn wrap(self) -> Json;
}

impl Wrap for Json {
    fn wrap(self) -> Json {
        Json::array(vec![self])
    }
}

#[test]
fn combines_removals_append_and_a_shared_subtree_survivor_edit() {
    let row = |id: &str| json!({"id": id, "stable": {}, "detail": {"text": id}});
    let (a, b, c, d) = (row("a"), row("b"), row("c"), row("d"));
    let mut changed_c = json!({"id": "c", "detail": {"text": "changed"}});
    changed_c.set("stable", c.get("stable").unwrap().clone()).unwrap();
    let mut changed_c_ordered = JsonMap::new();
    for key in ["id", "stable", "detail"] {
        changed_c_ordered.insert(key, changed_c.get(key).unwrap().clone());
    }
    let changed_c = Json::object(changed_c_ordered);
    let appended = row("e");
    expect_diff(
        values(vec![a.clone(), b, c, d.clone()]),
        values(vec![a, changed_c, d, appended.clone()]),
        json!([["p", ["values"], 1, 1, []], ["a", ["values", 1, "detail", "text"], "hanged"]])
            .pushed(json!(["p", ["values"], 3, 0]).pushed(Json::array(vec![appended]))),
    );
}

#[test]
fn does_not_retain_a_removed_neighbor_payload() {
    for size in [256 * 1024, 1024 * 1024] {
        let payload = "x".repeat(size);
        let retained: Vec<Json> =
            ["a", "b", "c", "d", "e"].iter().map(|id| json!({"id": id, "payload": payload})).collect();
        let before = values(retained.clone());
        let after = values(vec![retained[0].clone(), retained[2].clone(), retained[4].clone()]);
        let operations = diff_revisions(&before, &after);
        assert_eq!(ops_json(&operations), json!([["p", ["values"], 1, 1, []], ["p", ["values"], 2, 1, []]]));
        assert!(ops_json(&operations).to_string().len() < 100);
        assert_eq!(apply_immutable(Some(&before), &operations).unwrap(), after);
        assert_eq!(before.get("values").unwrap().as_array().unwrap(), &retained);
    }
}

#[test]
fn keeps_push_pop_and_middle_removal_narrow() {
    for size in [1_001usize, 10_000] {
        let rows: Vec<Json> = (0..size).map(|value| json!({ "value": value })).collect();
        let appended = json!({ "value": size });
        let mut pushed = rows.clone();
        pushed.push(appended.clone());
        expect_diff(values(rows.clone()), values(pushed), json!([["p", ["values"], size, 0, [{"value": size}]]]));
        expect_diff(
            values(rows.clone()),
            values(rows[..size - 1].to_vec()),
            json!([["p", ["values"], size - 1, 1, []]]),
        );
        let middle = size / 2;
        let mut removed = rows.clone();
        removed.remove(middle);
        expect_diff(values(rows), values(removed), json!([["p", ["values"], middle, 1, []]]));
    }
}

#[test]
fn keeps_forty_thousand_row_sparse_edits_narrow() {
    let rows: Vec<Json> = (0..40_000).map(|value| json!({"value": value, "stable": {"value": value}})).collect();
    let mut after = rows.clone();
    let mut expected = Vec::new();
    for index in (100..after.len()).step_by(400) {
        let mut row = json!({ "value": -(index as i64) });
        row.set("stable", rows[index].get("stable").unwrap().clone()).unwrap();
        after[index] = row;
        expected.push(json!(["s", ["values", index, "value"], -(index as i64)]));
    }
    let operations = diff_revisions(&values(rows.clone()), &values(after.clone()));
    assert_eq!(ops_json(&operations), Json::array(expected));
    assert!(ops_json(&operations).to_string().len() < 7_500);
    assert_eq!(apply_immutable(Some(&values(rows)), &operations).unwrap(), values(after));
}

#[test]
fn keeps_a_reconstructed_large_array_leaf_edit_narrow() {
    let before: Vec<Json> = (0..1_000).map(|value| json!({"value": value, "label": format!("row-{value}")})).collect();
    let mut after: Vec<Json> = before.iter().map(|row| Json::parse(&row.to_string()).unwrap()).collect();
    after[700].set("label", "changed").unwrap();
    let operations = diff_revisions(&values(before.clone()), &values(after.clone()));
    assert_eq!(ops_json(&operations), json!([["s", ["values", 700, "label"], "changed"]]));
    assert_eq!(apply_immutable(Some(&values(before)), &operations).unwrap(), values(after));
}

#[test]
fn splices_an_ambiguous_equal_count_moved_and_edited_gap() {
    let left = json!({"value": "left"});
    let right = json!({"value": "right"});
    let before = vec![left.clone(), json!({"id": 1, "value": "a"}), json!({"id": 2, "value": "b"}), right.clone()];
    let after = vec![left, json!({"id": 2, "value": "edited"}), json!({"id": 1, "value": "also-edited"}), right];
    expect_diff(
        values(before),
        values(after),
        json!([["p", ["values"], 1, 2, [{"id": 2, "value": "edited"}, {"id": 1, "value": "also-edited"}]]]),
    );
}

#[test]
fn keeps_a_large_rotation_payload_free() {
    let rows: Vec<Json> = (0..10_000).map(|value| json!({ "value": value })).collect();
    let mut rotated = rows[1_000..].to_vec();
    rotated.extend_from_slice(&rows[..1_000]);
    let operations = diff_revisions(&values(rows.clone()), &values(rotated.clone()));
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].verb(), "m");
    assert!(ops_json(&operations).to_string().len() < 60_000);
    assert_eq!(apply_immutable(Some(&values(rows)), &operations).unwrap(), values(rotated));
}

#[test]
fn encodes_five_hundred_unshifts_without_snapshotting_retained_rows() {
    let retained: Vec<Json> = (0..10_000).map(|value| json!({"value": value, "payload": "x".repeat(100)})).collect();
    let inserted: Vec<Json> = (0..500).map(|value| json!({ "value": -value - 1 })).collect();
    let mut after = inserted.clone();
    after.extend(retained.iter().cloned());
    let operations = diff_revisions(&values(retained.clone()), &values(after.clone()));
    assert_eq!(ops_json(&operations), json!(["p", ["values"], 0, 0]).pushed(Json::array(inserted)).wrap());
    assert!(ops_json(&operations).to_string().len() < 20_000);
    assert_eq!(apply_immutable(Some(&values(retained)), &operations).unwrap(), values(after));
}

#[test]
fn bounds_wide_object_operation_emission_with_a_root_replacement() {
    let mut before = JsonMap::new();
    let mut after = JsonMap::new();
    for index in 0..20_000 {
        before.insert(format!("field{index}"), Json::from(0));
        after.insert(format!("field{index}"), Json::from(1));
    }
    let after = Json::object(after);
    assert_eq!(ops_json(&diff_revisions(&Json::object(before), &after)), json!([["r"]]).pushed_into(after));
}

trait PushedInto {
    fn pushed_into(self, value: Json) -> Json;
}

impl PushedInto for Json {
    fn pushed_into(mut self, value: Json) -> Json {
        self.at_mut(0).unwrap().push(value).unwrap();
        self
    }
}

#[test]
fn bounds_a_wide_normalized_array_fallback_with_a_root_replacement() {
    let before = values(vec![Json::from(0); 40_000]);
    let after = values(vec![Json::from(1); 40_000]);
    let operations = diff_revisions(&before, &after);
    assert_eq!(operations, vec![Op::Replace(after.clone())]);
    assert_eq!(apply_immutable(Some(&before), &operations).unwrap(), after);
}
