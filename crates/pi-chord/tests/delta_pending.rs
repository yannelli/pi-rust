//! Port of upstream `test/delta-pending.test.ts`: folding is invisible to a replica, the cap, size, and cost. The
//! identity, path-key count, and GC cases are not ported: a batch is moved in, and Rust has no collector to watch.

use pi_chord::delta::{
    Change, MAX_PENDING_OPS, Op, PendingOps, PendingTake, Seg, apply_immutable, decoder, diff_revisions, encoder,
    parse_ops, track,
};
use pi_chord::{Json, json};

fn ops(batch: Json) -> Vec<Op> {
    parse_ops(&batch).unwrap()
}

/// A seeded generator, so a failing sequence reproduces from its seed.
struct Random(u32);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(self.0) / 4_294_967_296.0
    }

    fn int(&mut self, max: usize) -> usize {
        (self.next() * max as f64) as usize
    }

    fn text(&mut self) -> &'static str {
        const TEXTS: [&str; 10] = ["a", "bc", " tok", "é", "😀", "\u{2028}", "日本語", "\"q\"", "\\", ""];
        TEXTS[self.int(TEXTS.len())]
    }

    fn key(&mut self) -> &'static str {
        const KEYS: [&str; 8] = ["a", "b", "0", "1", "01", "a.b", "x y", "text"];
        KEYS[self.int(KEYS.len())]
    }

    fn primitive(&mut self) -> Json {
        match self.int(4) {
            0 => Json::from(self.int(100) as u64),
            1 => Json::from(self.text()),
            2 => Json::Bool(self.next() < 0.5),
            _ => Json::Null,
        }
    }

    fn value(&mut self, depth: usize) -> Json {
        let roll = self.next();
        if depth > 2 || roll < 0.4 {
            return self.primitive();
        }
        if roll < 0.7 {
            let length = self.int(4);
            return Json::array((0..length).map(|_| self.value(depth + 1)).collect());
        }
        let mut object = json!({});
        for _ in 0..=self.int(4) {
            let (key, value) = (self.key(), self.value(depth + 1));
            object.set(key, value).unwrap();
        }
        object
    }
}

/// Every path under `node`, parents first.
fn paths(node: &Json, path: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
    let children: Vec<(Seg, &Json)> = match node {
        Json::Object(map) => map.iter().map(|(key, child)| (Seg::Key(key.clone()), child)).collect(),
        Json::Array(items) => {
            items.iter().enumerate().map(|(index, child)| (Seg::Index(index as u64), child)).collect()
        }
        _ => Vec::new(),
    };
    for (segment, child) in children {
        path.push(segment);
        out.push(path.clone());
        paths(child, path, out);
        path.pop();
    }
}

/// One random edit anywhere in the draft: appends through `append_text()` or assignment, trims, splices, reorders,
/// object edits.
fn mutate(change: &mut Change, rnd: &mut Random) {
    let mut all = Vec::new();
    paths(change.state().unwrap(), &mut Vec::new(), &mut all);
    if all.is_empty() {
        let (key, value) = (if rnd.next() < 0.5 { "a" } else { "b" }, rnd.value(0));
        change.state().unwrap().set(key, value).unwrap();
        return;
    }
    let path = all[rnd.int(all.len())].clone();
    let (parent, last) = path.split_at(path.len() - 1);
    let roll = rnd.next();
    let current = change.state().unwrap().pointer_mut(&path).unwrap().clone();
    let next = match (&current, &last[0]) {
        (Json::String(_), Seg::Key(key)) if roll < 0.3 => {
            let text = rnd.text();
            change.append_text(parent, key, text).unwrap();
            return;
        }
        (Json::String(text), _) if roll < 0.6 => Json::from(format!("{text}{}", rnd.text())),
        (Json::String(text), _) if roll < 0.8 => Json::from(text.chars().skip(rnd.int(3)).collect::<String>()),
        (Json::Array(items), _) => {
            let (value, mut items) = (rnd.value(1), items.as_ref().clone());
            let (index, length) = (rnd.next(), rnd.int(2));
            if roll < 0.3 {
                items.push(value);
            } else if roll < 0.55 {
                let at = (index * (items.len() + 1) as f64) as usize;
                let end = (at + length).min(items.len());
                items.splice(at..end, [value]);
            } else if roll < 0.7 {
                items.reverse();
            } else if roll < 0.85 && !items.is_empty() {
                let at = (index * items.len() as f64) as usize;
                items[at] = value;
            } else {
                items.clear();
            }
            Json::array(items)
        }
        (Json::Object(_), _) if roll < 0.75 => {
            let (key, value) = (rnd.key(), rnd.value(1));
            let target = change.state().unwrap().pointer_mut(&path).unwrap();
            if roll < 0.5 {
                target.set(key, value).unwrap();
            } else {
                target.remove(key);
            }
            return;
        }
        _ if rnd.next() < 0.7 => rnd.primitive(),
        _ => rnd.value(1),
    };
    *change.state().unwrap().pointer_mut(&path).unwrap() = next;
}

/// Feed the commits through one queue, taking at random points as a slow consumer would, and replay what was taken
/// on a replica, once as ops and once through Delta's encoder and decoder. Both must reach the final value.
fn check_sequence(initial: &Json, commits: &[(Vec<Op>, Json)], rnd: &mut Random) -> (usize, usize) {
    let mut queue = PendingOps::new();
    let mut taken = Vec::new();
    for (index, (batch, value)) in commits.iter().enumerate() {
        queue.push(batch.clone());
        if rnd.next() < 0.1 || index + 1 == commits.len() {
            taken.push((queue.take(), value.clone()));
        }
    }
    let final_value = commits.last().map_or_else(|| initial.clone(), |commit| commit.1.clone());
    let (mut encode, mut decode) = (encoder(), decoder());
    let mut replica = initial.clone();
    let seed = decode.decode(&encode.encode(&[Op::Replace(initial.clone())])).unwrap();
    let mut decoded = apply_immutable(None, &seed).unwrap();
    let mut folded = 0;
    for (batch, value) in taken {
        let batch = match batch {
            PendingTake::Ops(ops) => ops,
            PendingTake::Replace => vec![Op::Replace(value)],
        };
        folded += batch.len();
        replica = apply_immutable(Some(&replica), &batch).unwrap();
        decoded = apply_immutable(Some(&decoded), &decode.decode(&encode.encode(&batch)).unwrap()).unwrap();
    }
    assert_eq!(replica, final_value);
    assert_eq!(decoded, final_value);
    (commits.iter().map(|commit| commit.0.len()).sum(), folded)
}

#[test]
fn is_invisible_to_a_replica_tracker_edits_anywhere_append_text_splices_and_reorders() {
    let mut rnd = Random(1);
    let (mut ops, mut folded) = (0, 0);
    for _ in 0..400 {
        let initial = json!({ "a": rnd.value(0), "text": "x", "list": [rnd.value(1), "y"] });
        let mut tracker = track(initial.clone());
        let mut commits = Vec::new();
        for _ in 0..1 + rnd.int(30) {
            let prepared = if rnd.next() < 0.03 {
                let value = json!({ "a": rnd.value(0) });
                tracker.prepare_replace(value)
            } else {
                let mut change = tracker.begin_change();
                for _ in 0..1 + rnd.int(4) {
                    mutate(&mut change, &mut rnd);
                }
                change.prepare().unwrap()
            };
            tracker.adopt(&prepared).unwrap();
            commits.push((prepared.ops.clone(), tracker.value().clone()));
        }
        let (all, kept) = check_sequence(&initial, &commits, &mut rnd);
        ops += all;
        folded += kept;
    }
    // The sequences fold: a fuzzer whose ops never merge proves nothing.
    assert!((folded as f64) < ops as f64 * 0.9, "{folded} of {ops}");
}

#[test]
fn is_invisible_to_a_replica_diff_revisions_between_arbitrary_values() {
    let mut rnd = Random(4);
    for _ in 0..300 {
        let initial = rnd.value(0);
        let mut value = initial.clone();
        let mut commits = Vec::new();
        for _ in 0..30 {
            let next = if rnd.next() < 0.2 {
                rnd.value(0)
            } else {
                let mut tracker = track(json!({ "value": value.clone() }));
                let mut change = tracker.begin_change();
                for _ in 0..1 + rnd.int(3) {
                    mutate(&mut change, &mut rnd);
                }
                let prepared = change.prepare().unwrap();
                tracker.adopt(&prepared).unwrap();
                tracker.value().get("value").cloned().unwrap_or(Json::Null)
            };
            commits.push((diff_revisions(&value, &next), next.clone()));
            value = next;
        }
        check_sequence(&initial, &commits, &mut rnd);
    }
}

/// Push each op as its own batch, take, and check the replay; returns the folded ops.
fn fold_one_by_one(initial: Json, batch: Json) -> Vec<Op> {
    let all = ops(batch);
    let mut queue = PendingOps::new();
    for op in &all {
        queue.push(vec![op.clone()]);
    }
    let PendingTake::Ops(folded) = queue.take() else { panic!("unexpected overflow") };
    let expected = apply_immutable(Some(&initial), &all).unwrap();
    assert_eq!(apply_immutable(Some(&initial), &folded).unwrap(), expected);
    let (mut encode, mut decode) = (encoder(), decoder());
    let base = apply_immutable(None, &decode.decode(&encode.encode(&[Op::Replace(initial)])).unwrap()).unwrap();
    let decoded = decode.decode(&encode.encode(&folded)).unwrap();
    assert_eq!(apply_immutable(Some(&base), &decoded).unwrap(), expected);
    folded
}

#[test]
fn a_leaf_set_in_an_array_element_a_splice_before_it_the_same_path_set_again() {
    let sets = json!([["s", ["arr", 2, "x"], 1], ["p", ["arr"], 0, 0, [{ "x": 9 }]], ["s", ["arr", 2, "x"], 2]]);
    assert_eq!(fold_one_by_one(json!({ "arr": [{ "x": 0 }, { "x": 0 }, { "x": 0 }] }), sets.clone()), ops(sets));
    let appends =
        json!([["a", ["arr", 0, "x"], "1"], ["p", ["arr"], 0, 0, [{ "x": "new" }]], ["a", ["arr", 0, "x"], "2"]]);
    assert_eq!(fold_one_by_one(json!({ "arr": [{ "x": "a" }, { "x": "b" }] }), appends.clone()), ops(appends));
}

#[test]
fn t_then_a_and_appends_around_a_trim() {
    assert_eq!(fold_one_by_one(json!({ "s": "hello" }), json!([["t", ["s"], 2], ["a", ["s"], "!"]])).len(), 2);
    let around = json!([["a", ["s"], "AB"], ["t", ["s"], 3], ["a", ["s"], "C"]]);
    assert_eq!(fold_one_by_one(json!({ "s": "hello" }), around).len(), 3);
}

#[test]
fn an_object_s_at_a_parent_then_leaf_sets_below_it() {
    let below = json!([["s", ["o", "a"], 2], ["s", ["o"], { "a": 5, "b": 6 }], ["s", ["o", "a"], 7]]);
    assert_eq!(fold_one_by_one(json!({ "o": { "a": 1 } }), below).len(), 3);
    let folded = fold_one_by_one(
        json!({ "x": { "s": "a" } }),
        json!([["s", ["x"], { "s": "b" }], ["a", ["x", "s"], "c"], ["s", ["x", "s"], "d"]]),
    );
    assert_eq!(folded, ops(json!([["s", ["x"], { "s": "b" }], ["s", ["x", "s"], "d"]])));
}

#[test]
fn an_ancestor_primitive_s_between_leaf_ops_below_it() {
    let folded = fold_one_by_one(
        json!({ "o": { "a": "x", "b": "y" } }),
        json!([["a", ["o", "a"], "1"], ["s", ["o", "b"], "q"], ["s", ["o"], "flat"], ["a", ["o"], "!"]]),
    );
    assert_eq!(folded, ops(json!([["a", ["o", "a"], "1"], ["s", ["o", "b"], "q"], ["s", ["o"], "flat!"]])));
    let batch = json!([["s", ["a", "b"], 1], ["s", ["a"], null], ["s", ["a"], "x"]]);
    let folded = fold_one_by_one(json!({ "a": { "b": 0 } }), batch);
    assert_eq!(folded, ops(json!([["s", ["a", "b"], 1], ["s", ["a"], "x"]])));
}

#[test]
fn deletes_moves_and_splices_shift_what_a_path_names() {
    let deleted = json!([["s", ["arr", 2], "C"], ["d", ["arr", 0]], ["s", ["arr", 1], "Z"]]);
    assert_eq!(fold_one_by_one(json!({ "arr": ["a", "b", "c"] }), deleted).len(), 3);
    let moved = json!([["s", ["arr", 0], 10], ["m", ["arr"], [2, 1, 0]], ["s", ["arr", 0], 20]]);
    assert_eq!(fold_one_by_one(json!({ "arr": [1, 2, 3] }), moved).len(), 3);
    let spliced = json!([
        ["s", ["arr", 0, "x"], 1],
        ["p", ["arr"], 1, 0, [{ "x": 0 }]],
        ["s", ["arr", 0, "x"], 2],
        ["s", ["arr", 1, "x"], 3],
    ]);
    assert_eq!(fold_one_by_one(json!({ "arr": [{ "x": 0 }] }), spliced).len(), 4);
}

#[test]
fn keys_of_one_parent_never_collide() {
    fold_one_by_one(
        json!({ "a": { "b": "x" }, "a.b": "z" }),
        json!([["a", ["a", "b"], "1"], ["s", ["a.b"], "y"], ["a", ["a", "b"], "2"]]),
    );
    fold_one_by_one(
        json!({ "o": { "1": "a" }, "arr": ["x", "y"] }),
        json!([["s", ["o", "1"], "b"], ["s", ["arr", 1], "z"], ["s", ["o", "1"], "c"], ["s", ["arr", 1], "w"]]),
    );
}

#[test]
fn an_array_set_at_the_end_appends_and_a_later_set_of_that_index_folds_into_it() {
    let batch = json!([["s", ["arr", 1], 2], ["s", ["arr", 0], 9], ["s", ["arr", 1], 3]]);
    let folded = fold_one_by_one(json!({ "arr": [1] }), batch);
    assert_eq!(folded, ops(json!([["s", ["arr", 1], 3], ["s", ["arr", 0], 9]])));
}

#[test]
fn r_drops_everything_before_it() {
    let folded = fold_one_by_one(json!({ "a": 1 }), json!([["s", ["a"], 2], ["r", { "b": "x" }], ["a", ["b"], "y"]]));
    assert_eq!(folded, ops(json!([["r", { "b": "x" }], ["a", ["b"], "y"]])));
}

#[test]
fn joins_appends_and_folds_appends_into_a_string_set() {
    let folded = fold_one_by_one(
        json!({ "s": "" }),
        json!([["s", ["s"], "ab"], ["a", ["s"], "c"], ["s", ["t"], 1], ["a", ["s"], "d"]]),
    );
    assert_eq!(folded, ops(json!([["s", ["s"], "abcd"], ["s", ["t"], 1]])));
    let folded = fold_one_by_one(
        json!({ "s": "" }),
        json!([["a", ["s"], "😀"], ["s", ["n"], 1], ["a", ["s"], "é"], ["a", ["s"], "!"]]),
    );
    assert_eq!(folded, ops(json!([["a", ["s"], "😀é!"], ["s", ["n"], 1]])));
}

#[test]
fn interleaved_leaves_a_structural_op_then_the_same_leaves_again() {
    let folded = fold_one_by_one(
        json!({ "a": "", "b": "", "list": [0] }),
        json!([
            ["a", ["a"], "1"],
            ["a", ["b"], "1"],
            ["a", ["a"], "2"],
            ["p", ["list"], 1, 0, [1]],
            ["a", ["b"], "2"],
            ["a", ["a"], "3"],
        ]),
    );
    let expected = json!([
        ["a", ["a"], "12"],
        ["a", ["b"], "1"],
        ["p", ["list"], 1, 0, [1]],
        ["a", ["b"], "2"],
        ["a", ["a"], "3"],
    ]);
    assert_eq!(folded, ops(expected));
}

/// `count` set ops on distinct paths.
fn distinct_sets(count: usize, from: usize) -> Vec<Op> {
    let set = |index: usize| Op::Set(vec![Seg::key("k"), Seg::Index(index as u64)], Json::from(index as u64));
    (from..from + count).map(set).collect()
}

fn taken_len(take: PendingTake) -> usize {
    match take {
        PendingTake::Ops(ops) => ops.len(),
        PendingTake::Replace => panic!("replaced"),
    }
}

#[test]
fn keeps_max_pending_ops_and_replaces_past_them() {
    let mut queue = PendingOps::new();
    queue.push(Vec::new());
    queue.push(distinct_sets(MAX_PENDING_OPS, 0));
    assert_eq!(taken_len(queue.take()), MAX_PENDING_OPS);
    queue.push(distinct_sets(MAX_PENDING_OPS / 2, 0));
    queue.push(distinct_sets(MAX_PENDING_OPS / 2, MAX_PENDING_OPS / 2));
    assert_eq!(taken_len(queue.take()), MAX_PENDING_OPS);
    queue.push(distinct_sets(MAX_PENDING_OPS / 2, 0));
    queue.push(distinct_sets(MAX_PENDING_OPS / 2 + 1, MAX_PENDING_OPS / 2));
    assert_eq!(queue.take(), PendingTake::Replace);
}

#[test]
fn counts_ops_after_folding() {
    let mut queue = PendingOps::new();
    for _ in 0..3 * MAX_PENDING_OPS {
        queue.push(ops(json!([["a", ["text"], "x"]])));
    }
    let expected = vec![Op::Append(vec![Seg::key("text")], "x".repeat(3 * MAX_PENDING_OPS).into())];
    assert_eq!(queue.take(), PendingTake::Ops(expected));
}

#[test]
fn accumulates_nothing_once_replaced_until_taken_or_a_batch_starting_with_r() {
    let mut queue = PendingOps::new();
    queue.push(distinct_sets(1, 0));
    queue.push(distinct_sets(MAX_PENDING_OPS + 1, 0));
    queue.push(distinct_sets(3, 0));
    assert_eq!(queue.take(), PendingTake::Replace);
    assert_eq!(queue.take(), PendingTake::Ops(Vec::new()));

    queue.push(distinct_sets(1, 0));
    queue.push(distinct_sets(MAX_PENDING_OPS + 1, 0));
    let fresh = ops(json!([["r", { "k": [0] }]]));
    queue.push(fresh.clone());
    assert_eq!(queue.take(), PendingTake::Ops(fresh.clone()));
    queue.push(distinct_sets(1, 0));
    queue.push(fresh);
    queue.push(ops(json!([["s", ["n"], 1]])));
    assert_eq!(queue.take(), PendingTake::Ops(ops(json!([["r", { "k": [0] }], ["s", ["n"], 1]]))));
}

#[test]
fn hands_one_batch_over_as_received_whatever_its_size() {
    let mut queue = PendingOps::new();
    let batch = distinct_sets(MAX_PENDING_OPS + 1, 0);
    queue.push(batch.clone());
    assert_eq!(queue.take(), PendingTake::Ops(batch.clone()));
    queue.push(batch);
    queue.push(ops(json!([["s", ["n"], 1]])));
    assert_eq!(queue.take(), PendingTake::Replace);
}

#[test]
fn counts_pending_ops_after_folding_the_replacement_once_overflowed_and_nothing_once_taken() {
    let mut queue = PendingOps::new();
    assert_eq!(queue.size(), 0);
    queue.push(Vec::new());
    assert_eq!(queue.size(), 0);
    queue.push(ops(json!([["a", ["text"], "a"], ["s", ["n"], 1]])));
    assert_eq!(queue.size(), 2);
    queue.push(ops(json!([["a", ["text"], "b"], ["s", ["n"], 2], ["p", ["list"], 0, 0, [1]]])));
    assert_eq!(queue.size(), 3);
    queue.take();
    assert_eq!(queue.size(), 0);

    queue.push(distinct_sets(MAX_PENDING_OPS, 0));
    assert_eq!(queue.size(), MAX_PENDING_OPS);
    queue.push(ops(json!([["s", ["n"], 1]])));
    assert_eq!(queue.size(), 1);
    assert_eq!(queue.take(), PendingTake::Replace);
    assert_eq!(queue.size(), 0);
}

#[test]
fn keeps_one_op_per_leaf_path_however_many_commits_write_it() {
    let text = || vec![Seg::key("entries"), Seg::Index(0), Seg::key("text")];
    let tokens = || vec![Seg::key("meta"), Seg::key("tokens")];
    let mut queue = PendingOps::new();
    for index in 0..50_000u64 {
        queue.push(vec![Op::Append(text(), "tok ".into()), Op::Set(tokens(), Json::from(index))]);
    }
    let expected = vec![Op::Append(text(), "tok ".repeat(50_000).into()), Op::Set(tokens(), Json::from(49_999u64))];
    assert_eq!(queue.take(), PendingTake::Ops(expected));
}
