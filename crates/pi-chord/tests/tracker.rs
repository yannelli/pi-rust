//! Port of upstream `test/delta-tracker/tracker.test.ts`: lifecycle and policy view sections.
//! The Rust draft is a copy-on-write `&mut Json`, so proxy-only cases are not ported here.

use pi_chord::delta::{Op, Seg, Tracker, apply, decoder, encoder, parse_ops, track};
use pi_chord::{ChordError, Json, json, path};

fn deep(value: &Json) -> Json {
    Json::parse(&value.to_string()).unwrap()
}

fn ops_json(ops: &[Op]) -> Json {
    Json::array(ops.iter().map(Op::to_json).collect())
}

/// Apply a JSON round trip of `ops` to a deep copy of `base`, like upstream `replay`.
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

/// Run one change and check replay, base immutability, and adoption by pointer swap.
fn settle(tracker: &mut Tracker, mutate: impl FnOnce(&mut Json)) -> Json {
    let base_root = tracker.value().clone();
    let base = deep(&base_root);
    let mut change = tracker.begin_change();
    mutate(change.state().unwrap());
    let prepared = change.prepare().unwrap();
    assert_eq!(replay(&base, &prepared.ops), prepared.value);
    assert_eq!(base_root, base);
    tracker.adopt(&prepared).unwrap();
    assert_eq!(base_root, base);
    assert!(tracker.value().same_ref(&prepared.value));
    prepared.value.clone()
}

#[test]
fn materializes_an_immutable_next_revision_and_counts_adoptions() {
    let initial = json!({"count": 1, "nested": {"text": "a"}, "values": [1]});
    let snapshot = deep(&initial);
    let mut tracker = track(initial.clone());
    assert_eq!(tracker.revision(), 0);
    let mut change = tracker.begin_change();
    change.state().unwrap().set("count", 2).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.base_revision(), 0);
    assert!(prepared.base.same_ref(&initial));
    assert!(!prepared.value.same_ref(&initial));
    assert!(tracker.value().same_ref(&initial));
    tracker.adopt(&prepared).unwrap();
    assert_eq!(tracker.revision(), 1);
    assert_eq!(initial, snapshot);
    assert!(prepared.base.same_ref(&initial));
    assert!(tracker.value().same_ref(&prepared.value));
    assert!(matches!(change.state(), Err(ChordError::Type(_))));
    assert_eq!(tracker.begin_change().prepare().unwrap().base_revision(), 1);
}

#[tokio::test]
async fn keeps_a_transaction_live_across_await_and_seals_it_at_prepare() {
    let mut tracker = track(json!({"left": 0, "nested": {"right": 0}}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("left", 1).unwrap();
    tokio::task::yield_now().await;
    change.state().unwrap().get_mut("nested").unwrap().set("right", 2).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value, json!({"left": 1, "nested": {"right": 2}}));
    assert!(matches!(change.state(), Err(ChordError::Type(_))));
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn reports_the_draft_of_a_prepared_change_as_settled() {
    let mut tracker = track(json!({"value": 0}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("value", 1).unwrap();
    let prepared = change.prepare().unwrap();
    let error = change.state().unwrap_err();
    assert!(error.to_string().contains("settled"), "{error}");
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn seals_a_large_draft_after_prepare() {
    let rows: Vec<Json> = (0..4_100).map(|value| json!({"value": value})).collect();
    let mut tracker = track(json!({"rows": rows}));
    let mut change = tracker.begin_change();
    for row in change.peek().unwrap().get("rows").unwrap().as_array().unwrap() {
        assert!(row.get("value").unwrap().as_f64().unwrap() >= 0.0);
    }
    put(change.state().unwrap(), &path!["rows", 0usize, "value"], -1);
    let prepared = change.prepare().unwrap();
    assert!(change.state().is_err());
    tracker.adopt(&prepared).unwrap();
    assert_eq!(tracker.value().pointer(&path!["rows", 0usize, "value"]), Some(&json!(-1)));
}

#[test]
fn invalidates_competing_open_overlays_without_changing_their_base() {
    let initial = json!({"rows": [{"value": 0}, {"value": 1}]});
    let mut tracker = track(initial.clone());
    let mut stale = tracker.begin_change();
    put(stale.state().unwrap(), &path!["rows", 1usize, "value"], 2);
    let mut winner = tracker.begin_change();
    put(winner.state().unwrap(), &path!["rows", 0usize, "value"], 3);
    let prepared = winner.prepare().unwrap();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(initial, json!({"rows": [{"value": 0}, {"value": 1}]}));
    assert_eq!(tracker.value(), &json!({"rows": [{"value": 3}, {"value": 1}]}));
    assert!(stale.state().unwrap_err().to_string().contains("settled"));
    assert!(stale.peek().unwrap_err().to_string().contains("settled"));
    assert!(stale.prepare().unwrap_err().to_string().contains("settled"));
    stale.abort();
}

#[test]
fn adopts_object_edits_as_ordinary_revisions() {
    let initial = json!({"first": 1, "second": 2});
    let mut tracker = track(initial.clone());
    let value = settle(&mut tracker, |draft| {
        draft.set("first", 3).unwrap();
        draft.remove("second");
        draft.set("third", 4).unwrap();
    });
    assert!(!tracker.value().same_ref(&initial));
    assert_eq!(initial, json!({"first": 1, "second": 2}));
    assert_eq!(value, json!({"first": 3, "third": 4}));
    assert_eq!(keys(tracker.value()), ["first", "third"]);
}

#[test]
fn supports_adopt_then_publish_ordering_and_held_publications() {
    let mut tracker = track(json!({"value": 0, "nested": {"count": 0}}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("value", 1).unwrap();
    put(change.state().unwrap(), &path!["nested", "count"], 1);
    let prepared = change.prepare().unwrap();
    let operations = prepared.ops.clone();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(prepared.ops, operations);
    assert_eq!(operations.len(), 2);
    let published = prepared.value.clone();
    assert!(published.same_ref(tracker.value()));
    assert_eq!(deep(&published), json!({"value": 1, "nested": {"count": 1}}));

    let mut next = tracker.begin_change();
    next.state().unwrap().set("value", 2).unwrap();
    let next_prepared = next.prepare().unwrap();
    tracker.adopt(&next_prepared).unwrap();
    assert!(!published.same_ref(tracker.value()));
    assert_eq!(prepared.value, json!({"value": 1, "nested": {"count": 1}}));
    assert_eq!(tracker.value(), &json!({"value": 2, "nested": {"count": 1}}));
}

#[test]
fn takes_o1_ownership_of_a_replacement_and_its_operation_payload() {
    let original = json!({"value": 0, "rows": []});
    let mut tracker = track(original.clone());
    let replacement = json!({"value": 1, "rows": [{"value": 2}]});
    let prepared = tracker.prepare_replace(replacement.clone());
    assert!(prepared.base.same_ref(&original));
    assert!(prepared.value.same_ref(&replacement));
    let [Op::Replace(payload)] = prepared.ops.as_slice() else { panic!("expected one replacement") };
    assert!(payload.same_ref(&replacement));
    tracker.adopt(&prepared).unwrap();
    assert!(prepared.base.same_ref(&original));
    assert!(prepared.value.same_ref(&replacement));
    assert!(tracker.value().same_ref(&replacement));
}

#[test]
fn keeps_aborted_and_stale_candidates_readable() {
    let mut tracker = track(json!({"value": 0}));
    let mut aborted_change = tracker.begin_change();
    aborted_change.state().unwrap().set("value", 1).unwrap();
    let aborted = aborted_change.prepare().unwrap();
    let aborted_view = aborted.value.clone();
    aborted.abort();
    assert_eq!(aborted_view.get("value"), Some(&json!(1)));
    assert_eq!(aborted.value.get("value"), Some(&json!(1)));
    assert!(tracker.adopt(&aborted).unwrap_err().to_string().contains("aborted"));

    let mut loser_change = tracker.begin_change();
    loser_change.state().unwrap().set("value", 2).unwrap();
    let loser = loser_change.prepare().unwrap();
    let loser_view = loser.value.clone();
    let winner = tracker.prepare_replace(json!({"value": 3}));
    tracker.adopt(&winner).unwrap();
    assert_eq!(tracker.value(), &json!({"value": 3}));
    assert_eq!(loser_view.get("value"), Some(&json!(2)));
    assert_eq!(loser.value.get("value"), Some(&json!(2)));
    assert!(tracker.adopt(&loser).unwrap_err().to_string().contains("stale"));
}

#[test]
fn lets_a_settled_change_abort_its_prepared_result_and_keep_it_readable() {
    let mut tracker = track(json!({"value": 0, "nested": {"value": 1}}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("value", 1).unwrap();
    let prepared = change.prepare().unwrap();
    let operations = prepared.ops.clone();
    change.abort();
    change.abort();
    assert_eq!(prepared.ops, operations);
    assert_eq!(prepared.value, json!({"value": 1, "nested": {"value": 1}}));
    assert!(tracker.adopt(&prepared).unwrap_err().to_string().contains("aborted"));
}

#[test]
fn rejects_foreign_and_aborted_prepared_values() {
    let mut first = track(json!({"value": 0}));
    let mut second = track(json!({"value": 0}));
    let prepared = first.prepare_replace(json!({"value": 1}));
    assert!(second.adopt(&prepared).unwrap_err().to_string().contains("different tracker"));
    prepared.abort();
    prepared.abort();
    assert!(first.adopt(&prepared).unwrap_err().to_string().contains("aborted"));
}

#[test]
fn reads_deleted_own_properties_as_absent() {
    let mut tracker = track(json!({"value": 1}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    state.remove("value");
    state.set("trackerInherited", 1).unwrap();
    state.remove("trackerInherited");
    assert!(state.get("value").is_none());
    assert!(state.get("trackerInherited").is_none());
    let prepared = change.prepare().unwrap();
    assert!(prepared.value.get("value").is_none());
    assert_eq!(ops_json(&prepared.ops), json!([["d", ["value"]]]));
    tracker.adopt(&prepared).unwrap();
    assert!(keys(tracker.value()).is_empty());
}

#[test]
fn normalizes_deeply_equal_object_and_array_assignments_to_committed_identity() {
    let initial = json!({"object": {"nested": {"value": 1}}, "array": [{"value": 1}], "rows": [{"value": 1}]});
    let mut tracker = track(initial.clone());
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    state.set("object", json!({"nested": {"value": 1}})).unwrap();
    state.set("array", json!([{"value": 1}])).unwrap();
    put(state, &path!["rows", 0usize], json!({"value": 1}));
    assert!(!state.get("object").unwrap().same_ref(initial.get("object").unwrap()));
    assert!(!state.get("array").unwrap().same_ref(initial.get("array").unwrap()));
    assert_eq!(state, &initial);
    let prepared = change.prepare().unwrap();
    assert!(prepared.ops.is_empty());
    tracker.adopt(&prepared).unwrap();
    assert!(tracker.value().same_ref(&initial));
    for key in ["object", "array"] {
        assert!(tracker.value().get(key).unwrap().same_ref(initial.get(key).unwrap()));
    }
    let row = path!["rows", 0usize];
    assert!(tracker.value().pointer(&row).unwrap().same_ref(initial.pointer(&row).unwrap()));
}

#[test]
fn supports_reads_keys_iteration_and_serialization_of_the_draft() {
    let mut tracker = track(json!({"values": [1, 2, 3], "object": {"a": 1}}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    items(state, "values").splice(1..2, [json!(4), json!(5)]);
    state.get_mut("object").unwrap().set("b", 2).unwrap();
    let values = state.get("values").unwrap().as_array().unwrap();
    assert_eq!(values.len(), 4);
    assert_eq!(Json::array(values.clone()), json!([1, 4, 5, 3]));
    let doubled: Vec<f64> = values.iter().map(|value| value.as_f64().unwrap() * 2.0).collect();
    assert_eq!(doubled, [2.0, 8.0, 10.0, 6.0]);
    assert_eq!(values.get(2), Some(&json!(5)));
    assert_eq!(keys(state.get("object").unwrap()), ["a", "b"]);
    assert_eq!(deep(state), json!({"values": [1, 4, 5, 3], "object": {"a": 1, "b": 2}}));
    change.abort();
}

#[test]
fn keeps_document_keys_named_like_overlay_fields() {
    let fields = json!({"context": 1, "base": 2, "parent": 3, "dirty": 4, "target": 5, "proxy": 6});
    let mut tracker = track(json!({"object": fields}));
    let mut change = tracker.begin_change();
    let object = change.state().unwrap().get_mut("object").unwrap();
    object.set("context", 7).unwrap();
    object.set("proxy", 8).unwrap();
    assert_eq!(keys(object), ["context", "base", "parent", "dirty", "target", "proxy"]);
    let prepared = change.prepare().unwrap();
    assert_eq!(
        prepared.value.get("object").unwrap(),
        &json!({"context": 7, "base": 2, "parent": 3, "dirty": 4, "target": 5, "proxy": 8})
    );
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn keeps_string_operation_forms_through_the_codec() {
    let mut tracker = track(json!({"text": "abcdefgh", "values": [1, 2, 3], "marker": 0}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    let text = state.get("text").unwrap().as_str().unwrap().to_string();
    state.set("text", format!("{}xyz", &text[3..])).unwrap();
    items(state, "values").splice(1..2, [json!(4)]);
    state.set("marker", 1).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value, json!({"text": "defghxyz", "values": [1, 4, 3], "marker": 1}));
    let listed = ops_json(&prepared.ops);
    let listed = listed.as_array().unwrap();
    assert!(listed.contains(&json!(["t", ["text"], 3])));
    assert!(listed.contains(&json!(["a", ["text"], "xyz"])));
    let decoded = decoder().decode(&encoder().encode(&prepared.ops)).unwrap();
    assert_eq!(replay(tracker.value(), &decoded), prepared.value);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn emits_deletion_and_supports_root_arrays() {
    let mut objects = track(json!({"keep": 1, "remove": 2}));
    settle(&mut objects, |draft| {
        draft.remove("remove");
    });
    assert_eq!(objects.value(), &json!({"keep": 1}));

    let mut arrays = track(json!([1, 2, 3]));
    settle(&mut arrays, |draft| {
        let values = draft.as_array_mut().unwrap();
        values.reverse();
        values.push(json!(4));
    });
    assert_eq!(arrays.value(), &json!([3, 2, 1, 4]));
}

#[test]
fn preserves_object_key_order_after_delete_and_re_add() {
    let mut tracker = track(json!({"first": 1, "second": 2}));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    state.remove("first");
    state.set("first", 1).unwrap();
    assert_eq!(keys(state), ["second", "first"]);
    let prepared = change.prepare().unwrap();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(keys(tracker.value()), ["second", "first"]);
}

#[test]
fn orders_new_integer_like_keys_before_strings() {
    let mut tracker = track(json!({"object": {"label": "x"}, "first": ""}));
    let base = deep(tracker.value());
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    let object = state.get_mut("object").unwrap();
    object.set("2", "two").unwrap();
    object.set("1", "one").unwrap();
    assert_eq!(keys(object), ["1", "2", "label"]);
    let first = keys(object)[0].clone();
    state.set("first", first).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value.get("first"), Some(&json!("1")));
    assert_eq!(keys(prepared.value.get("object").unwrap()), ["1", "2", "label"]);
    let replayed = replay(&base, &prepared.ops);
    assert_eq!(replayed, prepared.value);
    assert_eq!(replayed.to_string(), prepared.value.to_string());
    tracker.adopt(&prepared).unwrap();
    assert_eq!(tracker.value().get("first"), Some(&json!("1")));
    assert_eq!(keys(tracker.value().get("object").unwrap()), ["1", "2", "label"]);
}

#[test]
fn normalizes_deeply_equal_container_assignments_before_materialization() {
    let initial = json!({"child": {"a": 1, "b": 2}, "count": 0});
    let mut tracker = track(initial.clone());
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    state.set("child", json!({"b": 2, "a": 1})).unwrap();
    state.set("count", 1).unwrap();
    let prepared = change.prepare().unwrap();
    let replayed = replay(&initial, &prepared.ops);
    // A copy-on-write draft cannot tell this assignment from a delete and re-add, so the draft's
    // key order is kept (upstream keeps the base order here); value and replay still agree.
    assert_eq!(ops_json(&prepared.ops).as_array().unwrap().last(), Some(&json!(["s", ["count"], 1])));
    assert_eq!(keys(prepared.value.get("child").unwrap()), ["b", "a"]);
    assert_eq!(keys(replayed.get("child").unwrap()), ["b", "a"]);
    assert_eq!(prepared.value, replayed);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn keeps_integer_and_string_ordering_across_deletion_and_re_addition() {
    let mut tracker = track(json!({"object": {"1": "one", "2": "two", "first": "a", "second": "b"}}));
    let expected = ["0", "1", "2", "3", "second", "first"];
    let mut change = tracker.begin_change();
    let object = change.state().unwrap().get_mut("object").unwrap();
    object.remove("2");
    object.set("2", "two").unwrap();
    object.remove("first");
    object.set("first", "a").unwrap();
    object.set("3", "three").unwrap();
    object.set("0", "zero").unwrap();
    assert_eq!(keys(object), expected);
    let prepared = change.prepare().unwrap();
    assert_eq!(keys(prepared.value.get("object").unwrap()), expected);
    tracker.adopt(&prepared).unwrap();
    assert_eq!(keys(tracker.value().get("object").unwrap()), expected);
}

#[test]
fn keeps_repeated_identical_writes() {
    let mut tracker = track(json!({"value": 0, "values": [0]}));
    settle(&mut tracker, |draft| {
        draft.set("value", 1).unwrap();
        draft.set("value", 1).unwrap();
        let values = items(draft, "values");
        values[0] = Json::Null;
        values[0] = Json::Null;
        values.push(json!(2));
        values[1] = Json::Null;
        values[1] = Json::Null;
    });
    assert_eq!(tracker.value(), &json!({"value": 1, "values": [null, null]}));
}

#[test]
fn allows_index_replacement_and_append() {
    let mut tracker = track(json!({"values": [1, 2]}));
    let mut change = tracker.begin_change();
    let values = items(change.state().unwrap(), "values");
    values[1] = json!(9);
    values.push(json!(3));
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value.get("values").unwrap(), &json!([1, 9, 3]));
    assert_eq!(replay(tracker.value(), &prepared.ops), prepared.value);
    tracker.adopt(&prepared).unwrap();
    assert_eq!(tracker.value().get("values").unwrap(), &json!([1, 9, 3]));
}

#[test]
fn supports_optional_deletion_null_growth_shrinking_and_default_sort() {
    let mut tracker = track(json!({"optional": "remove", "values": [3, 1, 2]}));
    settle(&mut tracker, |draft| {
        draft.remove("optional");
        let values = items(draft, "values");
        values.resize(5, Json::Null);
        assert_eq!(Json::array(values.clone()), json!([3, 1, 2, null, null]));
        values.truncate(4);
        // JavaScript's default sort compares the string forms.
        values.sort_by_key(|value| value.to_string());
    });
    assert_eq!(tracker.value(), &json!({"values": [1, 2, 3, null]}));
}
