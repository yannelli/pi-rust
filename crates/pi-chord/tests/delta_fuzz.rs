//! Ports of upstream `test/state-fuzz.test.ts` and the randomized tracker transactions.

use pi_chord::delta::{Op, apply, apply_immutable, parse_ops, track};
use pi_chord::{Json, json};

struct Random(u32);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let seed = self.0;
        let mut value = (seed ^ (seed >> 15)).wrapping_mul(1 | seed);
        value = (value.wrapping_add((value ^ (value >> 7)).wrapping_mul(61 | value))) ^ value;
        f64::from(value ^ (value >> 14)) / 4_294_967_296.0
    }
}

fn detached(ops: &[Op]) -> Vec<Op> {
    let text = Json::array(ops.iter().map(Op::to_json).collect()).to_string();
    parse_ops(&Json::parse(&text).unwrap()).unwrap()
}

fn items<'a>(document: &'a mut Json, key: &str) -> &'a mut Vec<Json> {
    document.get_mut(key).unwrap().as_array_mut().unwrap()
}

fn mutate(document: &mut Json, choice: u32, value: u64) {
    let item = || json!({"id": value, "text": format!("item-{value}"), "score": value % 7});
    let id = |row: &Json| row.get("id").unwrap().as_f64().unwrap();
    match choice {
        0 => document.get_mut("text").unwrap().push_str(&format!("-{value}")).unwrap(),
        1 => {
            let text = document.get("text").unwrap().as_str().unwrap().to_string();
            let start = text.len().min(2);
            document.set("text", format!("{}{value}", &text[start..])).unwrap();
        }
        2 => items(document, "items").push(item()),
        3 => items(document, "items").insert(0, item()),
        4 => {
            let rows = items(document, "items");
            if !rows.is_empty() {
                rows.remove(0);
            }
        }
        5 => {
            items(document, "items").pop();
        }
        6 => {
            let rows = items(document, "items");
            let len = rows.len() as u64;
            let index = if len == 0 { 0 } else { (value % (len + 1)) as usize };
            let remove = if len == 0 { 0 } else { (value % 2) as usize };
            let end = (index + remove).min(rows.len());
            rows.splice(index..end, [item()]);
        }
        7 => items(document, "items").reverse(),
        8 => items(document, "items").sort_by(|left, right| id(left).total_cmp(&id(right))),
        9 => {
            let rows = items(document, "items");
            if !rows.is_empty() {
                let index = (value % rows.len() as u64) as usize;
                rows[index].set("score", value).unwrap();
            }
        }
        10 => {
            let meta = document.get_mut("meta").unwrap();
            let revision = meta.get("revision").unwrap().as_i64().unwrap();
            meta.set("revision", revision + 1).unwrap();
            meta.set("label", format!("revision-{value}")).unwrap();
        }
        11 => {
            document.get_mut("meta").unwrap().remove("label");
        }
        12 => {
            let rows = items(document, "items");
            if rows.len() > 1 {
                rows[1] = Json::parse(&rows[0].to_string()).unwrap();
            }
        }
        _ => {
            let rows = items(document, "items");
            for row in rows.iter_mut().take(2) {
                *row = item();
            }
        }
    }
}

#[test]
fn converges_across_randomized_prepared_revisions() {
    for seed in 1..=100u32 {
        let mut rng = Random(seed);
        let initial = json!({
            "items": (0..4).map(|id| json!({"id": id, "text": format!("item-{id}"), "score": 0})).collect::<Vec<_>>(),
            "text": "start",
            "meta": {"revision": 0},
        });
        let mut tracker = track(initial.clone());
        let mut expected = Json::parse(&initial.to_string()).unwrap();
        let mut replica = Json::parse(&initial.to_string()).unwrap();
        for step in 0..100u64 {
            let choice = (rng.next() * 14.0) as u32;
            let value = u64::from(seed) * 1_000 + step;
            let base_root = tracker.value().clone();
            let base = Json::parse(&base_root.to_string()).unwrap();
            mutate(&mut expected, choice, value);
            let mut change = tracker.begin_change();
            mutate(change.state().unwrap(), choice, value);
            let prepared = change.prepare().unwrap();
            assert_eq!(base_root, base, "prepare base seed {seed} step {step} choice {choice}");
            assert!(prepared.base.same_ref(&base_root));
            replica = apply_immutable(Some(&replica), &detached(&prepared.ops)).unwrap();
            assert_eq!(replica, prepared.value, "replay seed {seed} step {step} choice {choice}");
            assert_eq!(replica.to_string(), prepared.value.to_string(), "order seed {seed} step {step}");
            tracker.adopt(&prepared).unwrap();
            assert_eq!(base_root, base);
            assert!(tracker.value().same_ref(&prepared.value));
            assert_eq!(tracker.value(), &expected, "state seed {seed} step {step} choice {choice}");
        }
    }
}

#[test]
fn matches_policy_detached_replay_and_adopted_state_after_multi_operation_transactions() {
    for seed in 1..=40u32 {
        let mut rng = Random(seed);
        let initial = json!({
            "items": (0..4).map(|id| json!({"id": id, "text": format!("item-{id}"), "score": 0})).collect::<Vec<_>>(),
            "text": "start",
            "meta": {"revision": 0},
        });
        let mut tracker = track(initial.clone());
        let mut expected = initial;
        for transaction in 0..25u64 {
            let base_root = tracker.value().clone();
            let base = Json::parse(&base_root.to_string()).unwrap();
            let mut change = tracker.begin_change();
            for operation in 0..5u64 {
                let choice = (rng.next() * 11.0) as u32;
                let value = u64::from(seed) * 10_000 + transaction * 10 + operation;
                mutate(&mut expected, choice, value);
                mutate(change.state().unwrap(), choice, value);
            }
            let prepared = change.prepare().unwrap();
            assert_eq!(base_root, base, "prepare changed base seed {seed} transaction {transaction}");
            assert_eq!(prepared.value, expected, "policy seed {seed} transaction {transaction}");
            let replayed = apply(Some(base.clone()), &detached(&prepared.ops)).unwrap();
            assert_eq!(replayed, prepared.value, "replay seed {seed} transaction {transaction}");
            tracker.adopt(&prepared).unwrap();
            assert_eq!(base_root, base);
            assert!(tracker.value().same_ref(&prepared.value));
        }
    }
}

#[test]
fn orders_many_reverse_depth_object_edits() {
    let mut initial = json!({"value": 0});
    for _ in 0..512 {
        let mut node = json!({"value": 0});
        node.set("next", initial).unwrap();
        initial = node;
    }
    let mut tracker = track(initial.clone());
    let mut change = tracker.begin_change();
    let mut depth = 0;
    let mut node = Some(change.state().unwrap());
    while let Some(current) = node {
        current.set("value", depth + 1).unwrap();
        depth += 1;
        node = current.get_mut("next");
    }
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.ops.len(), depth as usize);
    assert_eq!(apply(Some(initial), &detached(&prepared.ops)).unwrap(), prepared.value);
    tracker.adopt(&prepared).unwrap();
}

#[test]
fn handles_reserved_own_keys_by_folding_into_a_safe_ancestor() {
    let initial = Json::parse(r#"{"safe":{"__proto__":{"value":1}}}"#).unwrap();
    let mut tracker = track(initial);
    let mut change = tracker.begin_change();
    change.state().unwrap().get_mut("safe").unwrap().get_mut("__proto__").unwrap().set("value", 2).unwrap();
    let prepared = change.prepare().unwrap();
    assert_eq!(prepared.value.get("safe").unwrap().get("__proto__").unwrap().get("value"), Some(&json!(2)));
    assert!(prepared.ops.iter().all(|op| op.path().len() <= 1));
    assert_eq!(apply(Some(tracker.value().clone()), &detached(&prepared.ops)).unwrap(), prepared.value);
    tracker.adopt(&prepared).unwrap();
}
