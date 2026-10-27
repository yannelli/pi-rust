//! Port of upstream `test/delta-append-text.test.ts`. The heap and allocation cases are not ported: a Rust string
//! owns its bytes, and appending to a shared JSON string copies it.

use pi_chord::delta::{Change, Op, Seg, Tracker, apply_immutable, parse_ops, track};
use pi_chord::{Json, json};

fn doc() -> Json {
    json!({ "title": "t", "content": [{ "type": "text", "text": "hello", "tags": ["a"] }], "meta": { "note": "n" } })
}

fn ops(batch: Json) -> Vec<Op> {
    parse_ops(&batch).unwrap()
}

fn block(index: u64) -> Vec<Seg> {
    vec![Seg::key("content"), Seg::Index(index)]
}

/// Run `mutate` in one change, adopt it, and return its ops; checks that replaying the ops gives the new value.
fn commit(tracker: &mut Tracker, mutate: impl FnOnce(&mut Change)) -> Vec<Op> {
    let base = tracker.value().clone();
    let mut change = tracker.begin_change();
    mutate(&mut change);
    let prepared = change.prepare().unwrap();
    assert_eq!(apply_immutable(Some(&base), &prepared.ops).unwrap(), prepared.value);
    tracker.adopt(&prepared).unwrap();
    prepared.ops
}

fn text(tracker: &Tracker, index: usize) -> String {
    tracker.value()["content"][index]["text"].as_str().unwrap().to_owned()
}

#[test]
fn records_an_append_op_and_the_appended_value() {
    let mut tracker = track(doc());
    let before = tracker.value().clone();
    let appended = commit(&mut tracker, |change| change.append_text(&block(0), "text", " world").unwrap());
    assert_eq!(appended, ops(json!([["a", ["content", 0, "text"], " world"]])));
    assert_eq!(text(&tracker, 0), "hello world");
    assert_eq!(before["content"][0]["text"], json!("hello"));
    let title = commit(&mut tracker, |change| change.append_text(&[], "title", "!").unwrap());
    assert_eq!(title, ops(json!([["a", ["title"], "!"]])));
    let note = commit(&mut tracker, |change| change.append_text(&[Seg::key("meta")], "note", "ote").unwrap());
    assert_eq!(note, ops(json!([["a", ["meta", "note"], "ote"]])));
    let expected = json!({
        "title": "t!",
        "content": [{ "type": "text", "text": "hello world", "tags": ["a"] }],
        "meta": { "note": "note" },
    });
    assert_eq!(*tracker.value(), expected);
}

#[test]
fn joins_several_appends_to_one_key_in_one_change_into_one_op_next_to_other_changes() {
    let mut tracker = track(doc());
    let batch = commit(&mut tracker, |change| {
        change.append_text(&block(0), "text", " a").unwrap();
        change.state().unwrap().set("title", "new").unwrap();
        change.append_text(&block(0), "text", " b").unwrap();
        change.append_text(&block(0), "text", "").unwrap();
        change.append_text(&block(0), "text", " c").unwrap();
    });
    assert!(batch.contains(&ops(json!([["a", ["content", 0, "text"], " a b c"]]))[0]));
    assert!(batch.contains(&ops(json!([["s", ["title"], "new"]]))[0]));
    assert_eq!(batch.len(), 2);
    assert_eq!(text(&tracker, 0), "hello a b c");
}

fn set_text(change: &mut Change, value: impl Into<Json>) {
    *change.state().unwrap().pointer_mut(&[Seg::key("content"), Seg::Index(0), Seg::key("text")]).unwrap() =
        value.into();
}

#[test]
fn falls_back_to_the_string_comparison_when_the_same_key_is_also_assigned_or_deleted() {
    let mut tracker = track(doc());
    let joined = commit(&mut tracker, |change| {
        change.append_text(&block(0), "text", " x").unwrap();
        let current = change.state().unwrap()["content"][0]["text"].as_str().unwrap().to_owned();
        set_text(change, format!("{current} y"));
    });
    assert_eq!(joined, ops(json!([["a", ["content", 0, "text"], " x y"]])));
    let replaced = commit(&mut tracker, |change| {
        change.append_text(&block(0), "text", " z").unwrap();
        set_text(change, "replaced");
    });
    assert_eq!(replaced, ops(json!([["s", ["content", 0, "text"], "replaced"]])));
    let assigned = commit(&mut tracker, |change| {
        change.state().unwrap().set("title", "assigned").unwrap();
        change.append_text(&[], "title", " then appended").unwrap();
    });
    assert_eq!(assigned, ops(json!([["s", ["title"], "assigned then appended"]])));
    let deleted = commit(&mut tracker, |change| {
        change.append_text(&[Seg::key("meta")], "note", "x").unwrap();
        change.state().unwrap().get_mut("meta").unwrap().as_object_mut().unwrap().remove("note");
    });
    assert_eq!(deleted, ops(json!([["d", ["meta", "note"]]])));
}

#[test]
fn appends_inside_a_block_the_same_change_inserted_or_replaced() {
    let mut tracker = track(doc());
    commit(&mut tracker, |change| {
        let content = change.state().unwrap().get_mut("content").unwrap().as_array_mut().unwrap();
        content.push(json!({ "type": "text", "text": "new", "tags": [] }));
        change.append_text(&block(1), "text", " block").unwrap();
        let content = change.state().unwrap().get_mut("content").unwrap().as_array_mut().unwrap();
        content[0] = json!({ "type": "text", "text": "other", "tags": [] });
        change.append_text(&block(0), "text", " one").unwrap();
    });
    assert_eq!([text(&tracker, 0), text(&tracker, 1)], ["other one", "new block"]);
}

#[test]
fn rejects_what_a_caller_can_get_wrong() {
    let mut tracker = track(doc());
    let mut change = tracker.begin_change();
    let message = |result: pi_chord::ChordResult<()>| result.unwrap_err().to_string();
    assert_eq!(message(change.append_text(&[], "content", "x")), "appendText() needs a string at \"content\"");
    assert_eq!(message(change.append_text(&[], "missing", "x")), "appendText() needs a string at \"missing\"");
    assert_eq!(message(change.append_text(&[Seg::key("content")], "0", "x")), "appendText() needs a string at \"0\"");
    assert_eq!(message(change.append_text(&[Seg::key("title")], "x", "x")), "appendText() needs a draft");
    change.prepare().unwrap();
    assert_eq!(message(change.append_text(&block(0), "text", "x")), "Cannot use a settled overlay");
    let mut aborted = tracker.begin_change();
    aborted.abort();
    assert_eq!(message(aborted.append_text(&[], "title", "x")), "Cannot use a settled overlay");
}

#[test]
fn accepts_only_string_members() {
    let mut tracker = track(json!({ "type": "text", "tags": [], "signature": null }));
    let mut change = tracker.begin_change();
    assert!(change.append_text(&[], "type", "x").is_ok());
    assert!(change.append_text(&[], "signature", "x").is_err());
    assert!(change.append_text(&[], "tags", "x").is_err());
    change.abort();
}

#[test]
fn records_appends_inside_array_items_that_change_together_or_next_to_new_items() {
    let mut tracker = track(json!({ "content": [{ "text": "a" }, { "args": { "s": "x" } }] }));
    let args = [Seg::key("content"), Seg::Index(1), Seg::key("args")];
    let both = commit(&mut tracker, |change| {
        change.append_text(&block(0), "text", "b").unwrap();
        change.append_text(&args, "s", "y").unwrap();
    });
    assert_eq!(both, ops(json!([["a", ["content", 0, "text"], "b"], ["a", ["content", 1, "args", "s"], "y"]])));
    let grown = commit(&mut tracker, |change| {
        change.append_text(&block(0), "text", "c").unwrap();
        let content = change.state().unwrap().get_mut("content").unwrap();
        content.as_array_mut().unwrap().push(json!({ "text": "new" }));
    });
    assert_eq!(grown[0], ops(json!([["a", ["content", 0, "text"], "c"]]))[0]);
    assert_eq!(grown.len(), 2);
    assert_eq!(text(&tracker, 0), "abc");
}
