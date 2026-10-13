//! Port of upstream `test/delta-tracker/retention.test.ts` and its worker. Heap probes become
//! `Weak` handles on `Json` containers: a released revision has no strong `Arc` left.
//! Tracker `WeakRef`s and heap deltas map to ownership and per-revision handles.

use std::sync::{Arc, Weak};

use pi_chord::delta::{Change, Prepared, track};
use pi_chord::{Json, JsonMap, json, path};

enum Watch {
    Array(Weak<Vec<Json>>),
    Object(Weak<JsonMap>),
}

impl Watch {
    fn new(value: &Json) -> Self {
        match value {
            Json::Array(items) => Watch::Array(Arc::downgrade(items)),
            Json::Object(map) => Watch::Object(Arc::downgrade(map)),
            other => panic!("cannot watch a {}", other.type_name()),
        }
    }

    fn alive(&self) -> bool {
        match self {
            Watch::Array(weak) => weak.strong_count() > 0,
            Watch::Object(weak) => weak.strong_count() > 0,
        }
    }
}

fn rows(count: usize) -> Json {
    Json::array((0..count).map(|value| json!({ "value": value })).collect())
}

fn object(entries: Vec<(&str, Json)>) -> Json {
    let mut map = JsonMap::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Json::object(map)
}

fn payload_root(count: usize) -> Json {
    object(vec![("payload", object(vec![("rows", rows(count))]))])
}

fn watch_at(value: &Json, path: &[pi_chord::delta::Seg]) -> Watch {
    Watch::new(value.pointer(path).unwrap())
}

#[test]
fn releases_an_aborted_payload_and_its_draft_children() {
    let mut tracker = track(json!({"payload": null}));
    let mut change = tracker.begin_change();
    let payload = rows(50_000);
    let (rows_watch, row_watch) = (Watch::new(&payload), Watch::new(payload.at(0).unwrap()));
    change.state().unwrap().set("payload", object(vec![("rows", payload)])).unwrap();
    change.abort();
    assert!(!rows_watch.alive());
    assert!(!row_watch.alive());
    assert_eq!(tracker.value(), &json!({"payload": null}));
}

#[test]
fn releases_an_unadopted_prepared_candidate() {
    let mut tracker = track(json!({"payload": null}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("payload", object(vec![("rows", rows(50_000))])).unwrap();
    let prepared = change.prepare().unwrap();
    let watch = watch_at(&prepared.value, &path!["payload", "rows"]);
    drop(prepared);
    assert!(!watch.alive(), "the settled change must not keep the candidate");
    assert_eq!(tracker.value(), &json!({"payload": null}));
}

#[test]
fn releases_later_revisions_when_the_tracker_is_dropped_despite_settled_handles() {
    let mut tracker = track(json!({"payload": null}));
    let mut change = tracker.begin_change();
    change.state().unwrap().set("payload", json!({"rows": [{"value": 1}]})).unwrap();
    let settled: Prepared = change.prepare().unwrap();
    tracker.adopt(&settled).unwrap();
    let future = tracker.prepare_replace(payload_root(50_000));
    tracker.adopt(&future).unwrap();
    let watch = watch_at(&future.value, &path!["payload", "rows"]);
    drop(future);
    drop(tracker);
    assert!(!watch.alive());
    assert_eq!(settled.value, json!({"payload": {"rows": [{"value": 1}]}}));
    drop(change);
}

#[test]
fn releases_obsolete_revisions() {
    let mut tracker = track(payload_root(50_000));
    let watch = watch_at(tracker.value(), &path!["payload", "rows"]);
    for value in 0..100 {
        let prepared = tracker.prepare_replace(json!({"payload": {"rows": [{ "value": value }]}}));
        tracker.adopt(&prepared).unwrap();
    }
    assert!(!watch.alive());
    assert_eq!(tracker.value(), &json!({"payload": {"rows": [{"value": 99}]}}));
}

/// Commit an edit of the first row, returning the prepared result and the settled change.
fn commit_first_row(tracker: &mut pi_chord::delta::Tracker) -> (Prepared, Change) {
    let mut change = tracker.begin_change();
    change.state().unwrap().pointer_mut(&path!["payload", "rows", 0usize]).unwrap().set("value", -1).unwrap();
    let prepared = change.prepare().unwrap();
    tracker.adopt(&prepared).unwrap();
    (prepared, change)
}

fn replace_small(tracker: &mut pi_chord::delta::Tracker) {
    let replacement = tracker.prepare_replace(json!({"payload": {"rows": [{"value": 1}]}}));
    tracker.adopt(&replacement).unwrap();
}

#[test]
fn a_held_prepared_keeps_its_revisions_until_dropped() {
    let mut tracker = track(payload_root(50_000));
    let (prepared, change) = commit_first_row(&mut tracker);
    drop(change);
    let base = watch_at(&prepared.base, &path!["payload", "rows"]);
    let value = watch_at(&prepared.value, &path!["payload", "rows"]);
    replace_small(&mut tracker);
    assert!(base.alive() && value.alive(), "a held Prepared keeps its immutable revisions");
    drop(prepared);
    assert!(!base.alive());
    assert!(!value.alive());
}

#[test]
fn a_held_settled_change_releases_its_revisions() {
    let mut tracker = track(payload_root(50_000));
    let (prepared, mut change) = commit_first_row(&mut tracker);
    let base = watch_at(&prepared.base, &path!["payload", "rows"]);
    let value = watch_at(&prepared.value, &path!["payload", "rows"]);
    drop(prepared);
    replace_small(&mut tracker);
    assert!(!value.alive());
    assert!(!base.alive(), "the settled change still holds its base revision");
    assert!(change.state().unwrap_err().to_string().contains("settled"));
}

#[test]
fn a_held_stale_change_releases_the_obsolete_revision() {
    let mut tracker = track(payload_root(50_000));
    let mut stale = tracker.begin_change();
    stale.state().unwrap().pointer_mut(&path!["payload", "rows", 0usize]).unwrap().set("value", -1).unwrap();
    let obsolete = watch_at(tracker.value(), &path!["payload", "rows"]);
    replace_small(&mut tracker);
    assert!(stale.prepare().unwrap_err().to_string().contains("settled"));
    assert!(!obsolete.alive(), "the stale change still holds the obsolete revision");
}

#[derive(Clone, Copy, Debug)]
enum Placement {
    Write,
    Writes,
    Insert,
    Override,
}

fn place_and_replace(kind: Placement, held: &mut Vec<Change>) -> Watch {
    let mut tracker = track(object(vec![
        ("child", json!({"value": 0})),
        ("visited", rows(5_000)),
        ("first", Json::Null),
        ("second", Json::Null),
        ("rows", if matches!(kind, Placement::Override) { json!([null]) } else { json!([]) }),
    ]));
    let mut change = tracker.begin_change();
    let state = change.state().unwrap();
    let large = |fill: i32| Json::array(vec![Json::from(fill); 50_000]);
    match kind {
        Placement::Write => state.set("first", large(1)).unwrap(),
        Placement::Writes => {
            state.set("first", large(1)).unwrap();
            state.set("second", large(2)).unwrap();
        }
        Placement::Insert => state.get_mut("rows").unwrap().push(large(1)).unwrap(),
        Placement::Override => *state.pointer_mut(&path!["rows", 0usize]).unwrap() = large(1),
    }
    let prepared = change.prepare().unwrap();
    let payload = match kind {
        Placement::Write | Placement::Writes => prepared.value.get("first").unwrap(),
        Placement::Insert | Placement::Override => prepared.value.pointer(&path!["rows", 0usize]).unwrap(),
    };
    let watch = Watch::new(payload);
    tracker.adopt(&prepared).unwrap();
    let replacement = tracker
        .prepare_replace(json!({"child": {"value": 1}, "visited": [], "first": null, "second": null, "rows": []}));
    tracker.adopt(&replacement).unwrap();
    held.push(change);
    watch
}

#[test]
fn releases_large_placements_while_their_settled_change_is_held() {
    let mut held = Vec::new();
    let kinds = [Placement::Write, Placement::Writes, Placement::Insert, Placement::Override];
    for kind in kinds {
        let watch = place_and_replace(kind, &mut held);
        assert!(!watch.alive(), "{kind:?} placement is still alive");
    }
    for change in &mut held {
        assert!(change.state().unwrap_err().to_string().contains("settled"));
    }
}

#[test]
fn releases_each_superseded_revision_of_a_long_array() {
    let mut tracker = track(object(vec![("rows", rows(100_000))]));
    let mut watches = vec![Watch::new(tracker.value().get("rows").unwrap())];
    for value in 0..100 {
        let mut change = tracker.begin_change();
        let rows = change.state().unwrap().get_mut("rows").unwrap().as_array_mut().unwrap();
        rows.last_mut().unwrap().set("value", -value - 1).unwrap();
        let prepared = change.prepare().unwrap();
        tracker.adopt(&prepared).unwrap();
        watches.push(Watch::new(prepared.value.get("rows").unwrap()));
    }
    let (current, superseded) = watches.split_last().unwrap();
    assert!(current.alive());
    assert_eq!(superseded.iter().filter(|watch| watch.alive()).count(), 0);
    let last = tracker.value().get("rows").unwrap().as_array().unwrap().last().unwrap().clone();
    assert_eq!(last, json!({"value": -100}));
}

#[test]
fn folds_wide_rewrites_into_a_replacement_and_releases_old_roots() {
    let mut map = JsonMap::new();
    for index in 0..5_000 {
        map.insert(format!("field{index}"), Json::from(0));
    }
    let mut tracker = track(Json::object(map));
    let mut watches = vec![Watch::new(tracker.value())];
    for value in 1..=100 {
        let mut change = tracker.begin_change();
        let state = change.state().unwrap();
        for index in 0..5_000 {
            state.set(format!("field{index}"), value).unwrap();
        }
        let prepared = change.prepare().unwrap();
        assert_eq!(prepared.ops.len(), 1);
        assert_eq!(prepared.ops[0].verb(), "r");
        tracker.adopt(&prepared).unwrap();
        watches.push(Watch::new(&prepared.value));
    }
    let (current, superseded) = watches.split_last().unwrap();
    assert!(current.alive());
    assert_eq!(superseded.iter().filter(|watch| watch.alive()).count(), 0);
    assert_eq!(tracker.value().get("field0"), Some(&json!(100)));
}

#[test]
fn survives_lifecycle_churn() {
    let mut tracker = track(json!({"value": 0}));
    for index in 0..100_000 {
        let mut change = tracker.begin_change();
        change.state().unwrap().set("value", index).unwrap();
        change.abort();
    }
    let prepared = tracker.prepare_replace(json!({"value": 1}));
    tracker.adopt(&prepared).unwrap();
    assert_eq!(tracker.value(), &json!({"value": 1}));
}
