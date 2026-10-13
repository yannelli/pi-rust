//! Port of upstream `test/delta.test.ts`. Proxy-specific draft checks map onto `Change::state`.

use pi_chord::delta::{
    Op, apply, apply_immutable, assert_valid_op, assert_valid_wire_op, decoder, encoder, is_base, overlap, parse_op,
    parse_ops, parse_wire_ops, track,
};
use pi_chord::{ChordError, Json, json, path};

fn ops(value: Json) -> Vec<Op> {
    parse_ops(&value).unwrap()
}

fn ops_json(ops: &[Op]) -> Json {
    Json::array(ops.iter().map(Op::to_json).collect())
}

fn apply_json(target: Json, batch: Json) -> Result<Json, ChordError> {
    let batch = parse_ops(&batch)?;
    apply(Some(target), &batch)
}

#[test]
fn keeps_a_draft_alive_and_adopts_only_a_prepared_change() {
    let input = json!({"count": 1, "nested": {"text": "a"}, "values": [1]});
    let mut tracker = track(input.clone());
    assert!(tracker.value().same_ref(&input));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("count", 2).unwrap();
    change.state().unwrap().get_mut("nested").unwrap().get_mut("text").unwrap().push_str("b").unwrap();
    change.state().unwrap().get_mut("values").unwrap().push(2).unwrap();
    assert_eq!(tracker.value(), &input);
    let prepared = change.prepare().unwrap();
    assert!(prepared.base.same_ref(tracker.value()));
    assert_eq!(prepared.value, json!({"count": 2, "nested": {"text": "ab"}, "values": [1, 2]}));
    assert_eq!(
        ops_json(&prepared.ops),
        json!([["s", ["count"], 2], ["a", ["nested", "text"], "b"], ["p", ["values"], 1, 0, [2]]])
    );
    assert!(matches!(change.state(), Err(ChordError::Type(_))));
    tracker.adopt(&prepared).unwrap();
    assert!(tracker.value().same_ref(&prepared.value));
}

#[test]
fn aborts_without_changing_the_committed_value() {
    let mut tracker = track(json!({"child": {"value": 1}}));
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("child").unwrap().set("value", 2).unwrap();
    change.abort();
    assert_eq!(tracker.value().pointer(&path!["child", "value"]), Some(&json!(1)));
    assert!(change.state().is_err());
    change.abort();
    assert!(change.prepare().unwrap_err().to_string().contains("settled"));
    tracker.begin_change().abort();
}

#[test]
fn grows_arrays_with_explicit_nulls() {
    let mut tracker = track(json!({"values": [1, 2]}));
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("values").unwrap().as_array_mut().unwrap().resize(4, Json::Null);
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value.get("values").unwrap(), &json!([1, 2, null, null]));
    assert!(change.state().is_err());
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn normalizes_a_deep_noop_to_exact_previous_identity() {
    let mut tracker = track(json!({"value": {"nested": [1, 2]}}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("value", json!({"nested": [1, 2]})).unwrap();
    let prepared = change.prepare().unwrap();
    assert!(prepared.value.same_ref(&prepared.base));
    assert!(prepared.ops.is_empty());
    tracker.adopt(&prepared).unwrap();
    assert!(tracker.value().same_ref(&prepared.base));
}

#[test]
fn takes_ownership_of_replacement_input_and_normalizes_noops() {
    let mut tracker = track(json!({"nested": {"value": 1}}));
    let replacement = json!({"nested": {"value": 2}});
    let prepared = tracker.prepare_replace(replacement.clone());
    assert!(prepared.value.same_ref(&replacement));
    assert_eq!(prepared.ops, vec![Op::Replace(replacement)]);
    tracker.adopt(&prepared).unwrap();
    let noop = tracker.prepare_replace(json!({"nested": {"value": 2}}));
    assert!(noop.value.same_ref(tracker.value()));
    assert!(noop.ops.is_empty());
}

#[test]
fn shares_immutable_operation_placements_with_the_prepared_candidate() {
    let mut tracker = track(json!({"rows": []}));
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("rows").unwrap().push(json!({"id": 1})).unwrap();
    let prepared = change.prepare().unwrap();
    let Op::Splice(_, _, _, items) = &prepared.ops[0] else { panic!("expected splice") };
    assert!(items[0].same_ref(prepared.value.pointer(&path!["rows", 0usize]).unwrap()));
}

#[test]
fn rejects_foreign_stale_and_repeated_preparations() {
    let mut first = track(json!({"value": 0}));
    let mut second = track(json!({"value": 0}));
    let mut change = first.begin_change();
    let mut competing = first.begin_change();
    change.state().unwrap().set("value", 1).unwrap();
    competing.state().unwrap().set("value", 2).unwrap();
    let prepared = change.prepare().unwrap();
    let competing_prepared = competing.prepare().unwrap();
    assert!(second.adopt(&prepared).unwrap_err().to_string().contains("different tracker"));
    first.adopt(&prepared).unwrap();
    assert!(first.adopt(&prepared).unwrap_err().to_string().contains("already been used"));
    assert!(first.adopt(&competing_prepared).unwrap_err().to_string().contains("stale"));
    let stale = first.prepare_replace(json!({"value": 2}));
    let winner = first.prepare_replace(json!({"value": 3}));
    first.adopt(&winner).unwrap();
    assert!(first.adopt(&stale).unwrap_err().to_string().contains("stale"));
    assert!(first.adopt(&stale).unwrap_err().to_string().contains("stale"));
}

#[test]
fn invalidates_a_prepared_result_when_its_change_is_aborted() {
    let mut tracker = track(json!({"value": 0}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("value", 1).unwrap();
    let prepared = change.prepare().unwrap();
    change.abort();
    assert!(tracker.adopt(&prepared).unwrap_err().to_string().contains("aborted"));
    change.abort();
}

#[test]
fn makes_competing_same_base_preparations_stale_after_adopting_a_noop() {
    let mut tracker = track(json!({"value": {"count": 1}}));
    let first = tracker.prepare_replace(json!({"value": {"count": 1}}));
    let competing = tracker.prepare_replace(json!({"value": {"count": 1}}));
    assert!(first.value.same_ref(&first.base));
    tracker.adopt(&first).unwrap();
    assert!(tracker.adopt(&first).unwrap_err().to_string().contains("already been used"));
    assert!(tracker.adopt(&competing).unwrap_err().to_string().contains("stale"));
}

#[test]
fn invalidates_open_competing_drafts() {
    let mut tracker = track(json!({"value": 0}));
    let mut open = tracker.begin_change();
    let winner = tracker.prepare_replace(json!({"value": 1}));
    tracker.adopt(&winner).unwrap();
    assert!(open.state().unwrap_err().to_string().contains("settled"));
    assert!(open.prepare().is_err());
}

#[test]
fn emits_an_owned_root_replacement_for_large_input() {
    let rows: Vec<Json> = (0..10_000).map(|value| json!({"value": value, "stable": {"value": value}})).collect();
    let mut tracker = track(json!({"rows": rows}));
    let mut replacement = json!({});
    let mut copied = tracker.value().get("rows").unwrap().clone();
    let stable = copied.at(5_000).unwrap().get("stable").unwrap().clone();
    let mut row = json!({"value": -1});
    row.set("stable", stable).unwrap();
    *copied.at_mut(5_000).unwrap() = row;
    replacement.set("rows", copied).unwrap();
    let prepared = tracker.prepare_replace(replacement.clone());
    assert!(prepared.value.same_ref(&replacement));
    assert_eq!(apply_immutable(Some(&prepared.base), &prepared.ops).unwrap(), prepared.value);
}

#[test]
fn emits_append_and_rolling_window_operations() {
    let mut tracker = track(json!({"text": "abcdefgh"}));
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("text").unwrap().push_str("ij").unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(ops_json(&prepared.ops), json!([["a", ["text"], "ij"]]));
    tracker.adopt(&prepared).unwrap();
    let mut change = tracker.begin_change();
    let text = change.state().unwrap().get("text").unwrap().as_str().unwrap().to_string();
    change.state().unwrap().set("text", format!("{}xyz", &text[3..])).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(ops_json(&prepared.ops), json!([["t", ["text"], 3], ["a", ["text"], "xyz"]]));
}

#[test]
fn finds_bounded_overlaps() {
    assert_eq!(overlap("abcdefgh", "defghxyz", 65_536), 5);
    assert_eq!(overlap("abcdef", "defghi", 0), 0);
}

#[test]
fn applies_mutable_and_immutable_operations() {
    let operations = ops(json!([["a", ["text"], "b"], ["p", ["values"], 1, 1, [3, 4]], ["s", ["nested", "value"], 2]]));
    let base = json!({"text": "a", "values": [1, 2], "nested": {"value": 1}, "stable": {"value": 9}});
    let immutable = apply_immutable(Some(&base), &operations).unwrap();
    assert_eq!(immutable, json!({"text": "ab", "values": [1, 3, 4], "nested": {"value": 2}, "stable": {"value": 9}}));
    assert_eq!(base, json!({"text": "a", "values": [1, 2], "nested": {"value": 1}, "stable": {"value": 9}}));
    assert!(immutable.get("stable").unwrap().same_ref(base.get("stable").unwrap()));
    assert_eq!(apply(Some(Json::parse(&base.to_string()).unwrap()), &operations).unwrap(), immutable);
}

#[test]
fn supports_root_replacement_root_splice_and_permutations() {
    let base = ops(json!([["r", [1, 2, 3]]]));
    assert!(is_base(&base));
    let value = apply(None, &base).unwrap();
    let value = apply_json(value, json!([["p", [], 1, 1, [4]]])).unwrap();
    let value = apply_json(value, json!([["m", [], [2, 0, 1]]])).unwrap();
    assert_eq!(value, json!([3, 1, 4]));
}

fn is_unsafe(result: Result<Json, ChordError>) -> bool {
    matches!(result, Err(ChordError::UnsafePath { .. }))
}

#[test]
fn rejects_unsafe_and_malformed_paths() {
    assert!(is_unsafe(apply_json(json!({}), json!([["s", ["constructor", "prototype", "x"], true]]))));
    assert!(is_unsafe(apply_json(json!({"values": [1]}), json!([["s", ["values", 3], 2]]))));
    assert!(apply_json(json!({"value": 1}), json!([["a", ["value"], "x"]])).is_err());
    assert!(assert_valid_op(&json!(["s", "value", 1])).is_err());
    assert!(assert_valid_op(&json!(["m", [], [0, 0]])).unwrap_err().to_string().contains("bijection"));
    assert!(parse_op(&json!(["s", "bad-path", 1])).unwrap_err().to_string().contains("path"));
}

#[test]
fn validates_decoded_and_wire_vocabularies_separately() {
    assert_valid_op(&json!(["s", ["value"], 1])).unwrap();
    assert!(assert_valid_op(&json!(["s", 1])).is_err());
    assert_valid_wire_op(&json!(["s", 1])).unwrap();
    assert_valid_wire_op(&json!(["#", 0, ["value"]])).unwrap();
}

fn wire(value: Json) -> Vec<pi_chord::delta::WireOp> {
    parse_wire_ops(&value).unwrap()
}

fn wire_json(ops: &[pi_chord::delta::WireOp]) -> Json {
    Json::array(ops.iter().map(|op| op.to_json()).collect())
}

#[test]
fn codec_interns_paths_omits_adjacent_paths_and_round_trips() {
    let mut enc = encoder();
    let mut dec = decoder();
    let first = ops(json!([["t", ["nested", "text"], 1], ["a", ["nested", "text"], "x"]]));
    assert_eq!(dec.decode(&enc.encode(&first)).unwrap(), first);
    let second = ops(json!([["a", ["nested", "text"], "y"]]));
    let encoded = enc.encode(&second);
    assert_eq!(wire_json(&encoded), json!([["#", 0, ["nested", "text"]], ["a", 0, "y"]]));
    assert_eq!(dec.decode(&encoded).unwrap(), second);
}

#[test]
fn codec_resets_path_dictionaries_on_a_base() {
    let mut enc = encoder();
    enc.encode(&ops(json!([["s", ["value"], 1]])));
    enc.encode(&ops(json!([["s", ["value"], 2]])));
    assert_eq!(wire_json(&enc.encode(&ops(json!([["r", {"value": 3}]])))), json!([["r", {"value": 3}]]));
    assert_eq!(wire_json(&enc.encode(&ops(json!([["s", ["value"], 4]])))), json!([["s", ["value"], 4]]));
}

#[test]
fn codec_rejects_unresolved_short_forms_and_unsafe_interned_paths() {
    assert!(decoder().decode(&wire(json!([["a", "x"]]))).is_err());
    assert!(
        parse_wire_ops(&json!([["#", 0, ["__proto__"]], ["s", 0, true]]))
            .is_err_and(|error| matches!(error, ChordError::UnsafePath { .. }))
    );
}

#[test]
fn codec_omits_an_adjacent_repeated_path_and_interns_on_second_use() {
    let encoded = encoder().encode(&ops(json!([["s", ["value"], 1], ["s", ["value"], 2]])));
    assert_eq!(wire_json(&encoded), json!([["s", ["value"], 1], ["s", 2]]));
    let mut enc = encoder();
    assert_eq!(wire_json(&enc.encode(&ops(json!([["a", ["a", "deep"], "1"]])))), json!([["a", ["a", "deep"], "1"]]));
    assert_eq!(
        wire_json(&enc.encode(&ops(json!([["a", ["a", "deep"], "2"]])))),
        json!([["#", 0, ["a", "deep"]], ["a", 0, "2"]])
    );
}

#[test]
fn codec_does_not_collide_paths_containing_null_characters() {
    let operations = ops(json!([["s", ["a\u{0}b"], 1], ["s", ["a", "b"], 2]]));
    assert_eq!(decoder().decode(&encoder().encode(&operations)).unwrap(), operations);
}

#[test]
fn codec_clears_decoder_ids_on_a_base_batch() {
    let mut dec = decoder();
    dec.decode(&wire(json!([["#", 0, ["a"]], ["a", 0, "1"]]))).unwrap();
    dec.decode(&wire(json!([["r", {"a": ""}]]))).unwrap();
    assert!(dec.decode(&wire(json!([["a", 0, "2"]]))).is_err());
}

#[test]
fn codec_makes_batches_after_a_base_self_contained() {
    let mut enc = encoder();
    enc.encode(&ops(json!([["a", ["a", "deep"], "1"]])));
    enc.encode(&ops(json!([["a", ["a", "deep"], "2"]])));
    let base = enc.encode(&ops(json!([["r", {"a": {"deep": "x"}}]])));
    let after = enc.encode(&ops(json!([["a", ["a", "deep"], "3"]])));
    assert_eq!(wire_json(&after), json!([["a", ["a", "deep"], "3"]]));
    let mut dec = decoder();
    assert_eq!(ops_json(&dec.decode(&base).unwrap()), json!([["r", {"a": {"deep": "x"}}]]));
    assert_eq!(ops_json(&dec.decode(&after).unwrap()), json!([["a", ["a", "deep"], "3"]]));
}

#[test]
fn codec_round_trips_deterministic_mixed_operation_streams() {
    let batches: Vec<Vec<Op>> = (0..100)
        .map(|index| {
            ops(json!([
                ["s", ["rows", index, "value"], index],
                ["a", ["output"], index.to_string()],
                ["p", ["tail"], index, 0, [index]]
            ]))
        })
        .collect();
    let mut enc = encoder();
    let mut dec = decoder();
    for batch in &batches {
        let wire = enc.encode(batch);
        let reparsed = parse_wire_ops(&Json::parse(&wire_json(&wire).to_string()).unwrap()).unwrap();
        assert_eq!(&dec.decode(&reparsed).unwrap(), batch);
    }
}

#[test]
fn does_not_mutate_a_replacement_payload_targeted_by_a_later_operation() {
    let replacement = json!({"nested": {"value": 1}});
    let mut batch = vec![Op::Replace(replacement.clone())];
    batch.extend(ops(json!([["s", ["nested", "value"], 2]])));
    let next = apply_immutable(None, &batch).unwrap();
    assert_eq!(replacement.pointer(&path!["nested", "value"]), Some(&json!(1)));
    assert_eq!(next.pointer(&path!["nested", "value"]), Some(&json!(2)));
}

#[test]
fn rejects_reserved_paths_and_allows_reserved_names_inside_values() {
    assert!(apply_json(json!({}), json!([["s", ["constructor", "prototype", "gadget"], true]])).is_err());
    let value = Json::parse(r#"{"__proto__":{"z":1}}"#).unwrap();
    let out = apply(Some(json!({})), &[Op::Set(path!["value"], value)]).unwrap();
    assert!(out.get("value").unwrap().get("__proto__").is_some());
}

#[test]
fn writes_an_existing_index_and_appends_exactly_one_past_the_end() {
    assert_eq!(
        apply_json(json!({"values": [1, 2, 3]}), json!([["s", ["values", 1], 9]])).unwrap(),
        json!({"values": [1, 9, 3]})
    );
    assert_eq!(
        apply_json(json!({"values": [1, 2, 3]}), json!([["s", ["values", 3], 9]])).unwrap(),
        json!({"values": [1, 2, 3, 9]})
    );
}

#[test]
fn rejects_gaps_huge_indices_and_string_spelled_indices() {
    assert!(apply_json(json!({"values": [1, 2, 3]}), json!([["s", ["values", 5], 9]])).is_err());
    assert!(apply_json(json!({"values": []}), json!([["s", ["values", 4_294_967_290u64], 1]])).is_err());
    assert!(apply_json(json!({"values": [1]}), json!([["s", ["values", "0"], 9]])).is_err());
    assert!(apply_json(json!({"values": ["a"]}), json!([["a", ["values", "0"], "b"]])).is_err());
}

#[test]
fn allows_explicit_growth_values_and_rejects_deletion_past_the_end() {
    assert_eq!(
        apply_json(json!({"values": [1]}), json!([["p", ["values"], 1, 0, [null, null, 9]]])).unwrap(),
        json!({"values": [1, null, null, 9]})
    );
    assert!(apply_json(json!({"values": [1]}), json!([["d", ["values", 1]]])).is_err());
}

#[test]
fn applies_large_splice_payloads() {
    let items = vec![Json::Null; 300_000];
    let result = apply(Some(json!({"values": []})), &[Op::Splice(path!["values"], 0, 0, items)]).unwrap();
    assert_eq!(result.get("values").unwrap().as_array().unwrap().len(), 300_000);
}

#[test]
fn rejects_unknown_verbs_malformed_tuples_and_payloads() {
    for invalid in [
        json!(["ZZZ", ["value"], 9]),
        json!(["p", ["values"], 0, 0, "not-an-array"]),
        json!(["s", "value", 9]),
        json!({"op": "s"}),
        json!(null),
    ] {
        assert!(parse_op(&invalid).is_err(), "{invalid}");
    }
}

#[test]
fn rejects_invalid_append_and_truncation_operations() {
    assert!(apply_json(json!({"value": 1}), json!([["a", ["missing"], "x"]])).is_err());
    assert!(apply_json(json!({"value": 1}), json!([["a", ["value"], "x"]])).is_err());
    assert!(apply_json(json!({"value": "abc"}), json!([["t", ["value"], -1]])).is_err());
    assert!(parse_wire_ops(&json!([["t", ["value"], -1]])).is_err());
}

#[test]
fn clamps_splice_removal_past_the_end() {
    assert_eq!(
        apply_json(json!({"values": [1, 2]}), json!([["p", ["values"], 0, 1e9, []]])).unwrap(),
        json!({"values": []})
    );
}

#[test]
fn accepts_decoded_operations_and_rejects_wire_only_forms() {
    for operation in [
        json!(["r", {"value": 1}]),
        json!(["s", ["value"], 1]),
        json!(["d", ["value"]]),
        json!(["a", ["value"], "x"]),
        json!(["t", ["value"], 2]),
        json!(["p", ["value"], 0, 0, []]),
        json!(["m", ["value"], [0]]),
    ] {
        assert_valid_op(&operation).unwrap();
    }
    for wire_only in [
        json!(["s", 1]),
        json!(["d"]),
        json!(["a", "x"]),
        json!(["t", 2]),
        json!(["p", 0, 0, []]),
        json!(["#", 0, ["value"]]),
        json!(["s", 0, 1]),
    ] {
        assert!(assert_valid_op(&wire_only).is_err(), "{wire_only}");
        assert_valid_wire_op(&wire_only).unwrap();
    }
}
