//! Port of upstream `test/delta-apply-immutable.test.ts`.

use pi_chord::delta::{Op, apply, apply_immutable, apply_immutable_batches, parse_ops, track};
use pi_chord::{ChordError, Json, json, path};

fn ops(value: Json) -> Vec<Op> {
    parse_ops(&value).unwrap()
}

fn with(mut base: Json, key: &str, value: &Json) -> Json {
    base.set(key, value.clone()).unwrap();
    base
}

#[test]
fn appends_to_an_array_not_yet_copied_in_one_copy_sharing_everything_else() {
    let entries: Vec<Json> = (0..1_000).map(|index| json!({ "index": index })).collect();
    let base = json!({
        "log": { "entries": Json::array(entries.clone()), "meta": { "count": 1_000 } }, "other": { "value": 1 },
    });
    let appended = ops(json!([
        ["p", ["log", "entries"], 1_000, 0, [{ "index": 1_000 }]],
        ["p", ["log", "entries"], 1_001, 0, [{ "index": 1_001 }]],
    ]));
    let result = apply_immutable(Some(&base), &appended).unwrap();
    let list = |value: &Json| value.pointer(&path!["log", "entries"]).unwrap().as_array().unwrap().len();
    assert_eq!((list(&result), list(&base)), (1_002, 1_000));
    let last = result.pointer(&path!["log", "entries"]).unwrap().as_array().unwrap();
    assert_eq!(&last[1_000..], [json!({ "index": 1_000 }), json!({ "index": 1_001 })]);
    let before = base.pointer(&path!("log", "entries")).unwrap().as_array().unwrap();
    assert!(last[0].same_ref(&before[0]));
    assert!(result.pointer(&path!["log", "meta"]).unwrap().same_ref(base.pointer(&path!["log", "meta"]).unwrap()));
    assert!(result.get("other").unwrap().same_ref(base.get("other").unwrap()));
    assert!(!result.get("log").unwrap().same_ref(base.get("log").unwrap()));

    let root = json!([1, 2]);
    assert_eq!(apply_immutable(Some(&root), &ops(json!([["p", [], 2, 0, [3]]]))).unwrap(), json!([1, 2, 3]));
    assert_eq!(root, json!([1, 2]));
    // An appended item that is itself an array stays one element.
    assert_eq!(apply_immutable(Some(&root), &ops(json!([["p", [], 2, 0, [[3, 4]]]]))).unwrap(), json!([1, 2, [3, 4]]));
    // An insert that is not at the end takes the general path.
    assert_eq!(apply_immutable(Some(&root), &ops(json!([["p", [], 1, 0, [9]]]))).unwrap(), json!([1, 9, 2]));
}

#[test]
fn fails_an_append_on_an_invalid_path_with_the_general_paths_errors() {
    let document = json!({ "values": [], "list": [[1]], "text": "a" });
    let invalid = [
        json!(["p", ["values", "missing"], 0, 0, []]),
        json!(["p", ["missing"], 0, 0, []]),
        json!(["p", ["values", 0], 0, 0, []]),
        json!(["p", ["list", "x"], 0, 0, []]),
        json!(["p", ["text"], 0, 0, []]),
    ];
    for operation in invalid {
        let result = apply_immutable(Some(&document), &ops(json!([operation.clone()])));
        assert!(matches!(result, Err(ChordError::Path { .. } | ChordError::UnsafePath { .. })), "{operation}");
    }
    for operation in [json!(["p", ["__proto__"], 0, 0, []]), json!(["p", ["list", 0, "constructor"], 0, 0, []])] {
        let result = parse_ops(&json!([operation.clone()])).and_then(|ops| apply_immutable(Some(&document), &ops));
        assert!(matches!(result, Err(ChordError::UnsafePath { .. })), "{operation}: {result:?}");
    }
}

#[test]
fn copies_touched_containers_while_preserving_input_and_payload_ownership() {
    let shared = json!({"nested": {"value": 1}});
    let untouched = json!({"value": 9});
    let row_payload = json!({"id": 4, "label": "placed"});
    let base = json!({
        "text": "abcdef", "stable": {"value": 7}, "branch": {"value": 1}, "copy": null, "placed": null,
        "untouched": null, "left": null, "right": null, "meta": {"count": 0, "obsolete": true},
        "rows": [{"id": 1, "label": "one"}, {"id": 2, "label": "two"}, {"id": 3, "label": "three"}],
    });
    let mut operations = ops(json!([
        ["t", ["text"], 2],
        ["a", ["text"], "!"],
        ["s", ["meta", "count"], 1],
        ["s", ["meta", "count"], 2],
        ["d", ["meta", "obsolete"]],
    ]));
    operations.push(Op::Set(path!["copy"], base.get("branch").unwrap().clone()));
    operations.extend(ops(json!([["s", ["copy", "value"], 2]])));
    operations.push(Op::Set(path!["placed"], shared.clone()));
    operations.extend(ops(json!([["s", ["placed", "nested", "value"], 2]])));
    operations.push(Op::Set(path!["untouched"], untouched.clone()));
    operations.push(Op::Set(path!["left"], shared.clone()));
    operations.push(Op::Set(path!["right"], shared.clone()));
    operations.extend(ops(json!([["s", ["left", "nested", "value"], 3]])));
    operations.push(Op::Splice(path!["rows"], 1, 1, vec![row_payload.clone()]));
    operations.extend(ops(json!([
        ["s", ["rows", 1, "label"], "edited"],
        ["m", ["rows"], [1, 0, 2]],
        ["s", ["rows", 0, "label"], "moved"],
    ])));

    let result = apply_immutable(Some(&base), &operations).unwrap();
    let mutable = apply(Some(Json::parse(&base.to_string()).unwrap()), &operations).unwrap();
    assert_eq!(result, mutable);
    assert_eq!(result.get("text"), Some(&json!("cdef!")));
    assert!(result.get("stable").unwrap().same_ref(base.get("stable").unwrap()));
    assert!(result.get("untouched").unwrap().same_ref(&untouched));
    assert!(!result.get("copy").unwrap().same_ref(base.get("branch").unwrap()));
    assert_eq!(result.get("copy"), Some(&json!({"value": 2})));
    assert!(!result.get("placed").unwrap().same_ref(&shared));
    assert_eq!(result.get("placed"), Some(&json!({"nested": {"value": 2}})));
    assert!(!result.get("left").unwrap().same_ref(&shared));
    assert!(result.get("right").unwrap().same_ref(&shared));
    assert_eq!(result.pointer(&path!["rows", 0usize]), Some(&json!({"id": 4, "label": "moved"})));
    assert_eq!(base.pointer(&path!["branch", "value"]), Some(&json!(1)));
    assert_eq!(shared.pointer(&path!["nested", "value"]), Some(&json!(1)));
    assert_eq!(row_payload.get("label"), Some(&json!("placed")));
}

#[test]
fn protects_root_replacement_payloads_before_later_edits() {
    let replacement = json!({"nested": {"value": 1}, "values": [1, 2, 3]});
    let batches = [
        vec![Op::Replace(replacement.clone())],
        ops(json!([["s", ["nested", "value"], 2]])),
        ops(json!([["p", ["values"], 1, 1, [4, 5]], ["m", ["values"], [3, 0, 1, 2]]])),
    ];
    let result = apply_immutable_batches(None, batches.iter().map(Vec::as_slice)).unwrap();
    assert_eq!(result, json!({"nested": {"value": 2}, "values": [3, 1, 4, 5]}));
    assert_eq!(replacement, json!({"nested": {"value": 1}, "values": [1, 2, 3]}));
    let array = json!([1, 2, 3]);
    let array_result =
        apply_immutable(Some(&array), &ops(json!([["p", [], 1, 1, [4, 5]], ["m", [], [3, 0, 1, 2]], ["d", [1]]])))
            .unwrap();
    assert_eq!(array_result, json!([3, 4, 5]));
    assert_eq!(array, json!([1, 2, 3]));
}

#[test]
fn shares_one_copy_on_write_scope_across_batch_partitions() {
    let base = json!({"text": "abcdef", "meta": {"count": 0}, "values": [{"id": 1, "value": 1}, {"id": 2, "value": 2}, {"id": 3, "value": 3}]});
    let batches = [
        ops(json!([["s", ["meta", "count"], 1], ["p", ["values"], 1, 1, [{"id": 4, "value": 4}]]])),
        vec![],
        ops(json!([["m", ["values"], [2, 0, 1]], ["s", ["values", 2, "value"], 40]])),
        ops(json!([["t", ["text"], 2], ["a", ["text"], "!"]])),
    ];
    let intermediate = apply_immutable(Some(&base), &batches[0]).unwrap();
    let snapshot = intermediate.to_string();
    let mut sequential = intermediate.clone();
    for batch in &batches[1..] {
        sequential = apply_immutable(Some(&sequential), batch).unwrap();
    }
    let streamed = apply_immutable_batches(Some(&base), batches.iter().map(Vec::as_slice)).unwrap();
    let flattened = apply_immutable(Some(&base), &batches.concat()).unwrap();
    assert_eq!(streamed, sequential);
    assert_eq!(streamed, flattened);
    assert_eq!(intermediate.to_string(), snapshot);
    assert_eq!(base.pointer(&path!["meta", "count"]), Some(&json!(0)));
    assert_eq!(base.pointer(&path!["values", 1usize, "id"]), Some(&json!(2)));
}

#[test]
fn handles_tracker_produced_batches_across_revision_boundaries() {
    let initial = json!({
        "text": "start",
        "values": (0..8).map(|id| json!({"id": id, "score": 0})).collect::<Vec<_>>(),
        "revision": 0,
    });
    let mut tracker = track(initial.clone());
    let mut batches = Vec::new();
    for revision in 1..=40i64 {
        let mut change = tracker.begin_change();
        let state = change.state().unwrap();
        state.set("revision", revision).unwrap();
        let text = state.get("text").unwrap().as_str().unwrap().to_string();
        state.set("text", format!("{}{revision}", &text[1..])).unwrap();
        let values = state.get_mut("values").unwrap().as_array_mut().unwrap();
        match revision % 5 {
            0 => values.reverse(),
            1 => values.push(json!({"id": 100 + revision, "score": revision})),
            2 => {
                values.remove(0);
            }
            3 => {
                let index = revision as usize % values.len();
                values[index].set("score", revision).unwrap();
            }
            _ => {
                values.splice(1..2, [json!({"id": 200 + revision, "score": revision})]);
            }
        }
        let prepared = change.prepare().unwrap();
        batches.push(prepared.ops.clone());
        tracker.adopt(&prepared).unwrap();
    }
    assert_eq!(&apply_immutable_batches(Some(&initial), batches.iter().map(Vec::as_slice)).unwrap(), tracker.value());
}

#[test]
fn does_not_expose_partial_application_when_validation_fails() {
    let base = json!({"nested": {"value": 1}});
    let invalid = [
        ops(json!([["s", ["nested", "value"], 2]])),
        vec![Op::Set(path!["constructor", "prototype", "polluted"], true.into())],
    ];
    assert!(apply_immutable_batches(Some(&base), invalid.iter().map(Vec::as_slice)).is_err());
    assert_eq!(base.pointer(&path!["nested", "value"]), Some(&json!(1)));
    let unsafe_batches =
        [ops(json!([["s", ["nested", "value"], 4]])), vec![Op::Set(path!["__proto__", "polluted"], true.into())]];
    let error = apply_immutable_batches(Some(&base), unsafe_batches.iter().map(Vec::as_slice)).unwrap_err();
    assert!(matches!(error, ChordError::UnsafePath { .. }));
    assert_eq!(base.pointer(&path!["nested", "value"]), Some(&json!(1)));
    let missing =
        apply_immutable(Some(&json!({"values": []})), &ops(json!([["s", ["values", "missing", "value"], 1]])));
    assert!(matches!(missing, Err(ChordError::UnsafePath { .. }) | Err(ChordError::Path { .. })));
    let string_index =
        apply_immutable(Some(&json!({"values": [{}]})), &ops(json!([["s", ["values", "0", "value"], 1]])));
    assert!(matches!(string_index, Err(ChordError::UnsafePath { .. })));
}

#[test]
fn allows_one_immutable_batch_to_fan_out_without_mutating_shared_payloads() {
    let payload = json!({"nested": {"value": 1}});
    let mut operations = vec![Op::Set(path!["placed"], payload.clone())];
    operations.extend(ops(json!([["s", ["placed", "nested", "value"], 2]])));
    let base = with(json!({}), "placed", &Json::Null);
    let first = apply_immutable(Some(&base), &operations).unwrap();
    let second = apply_immutable(Some(&base), &operations).unwrap();
    assert_eq!(first, second);
    assert!(!first.same_ref(&second));
    assert!(!first.get("placed").unwrap().same_ref(second.get("placed").unwrap()));
    assert_eq!(payload.pointer(&path!["nested", "value"]), Some(&json!(1)));
}
