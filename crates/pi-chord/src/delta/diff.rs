//! Compact operation batches computed from two immutable JSON revisions.

use std::collections::HashMap;
use std::sync::Arc;

use super::diff_array::diff_array;
use super::{Op, Path, Seg, is_reserved, overlap};
use crate::json::{Json, JsonMap, array_index_key, format_js_number, utf16_len, utf16_offset};

const DEFAULT_OVERLAP_SCAN: usize = 65_536;
const MAX_DELTA_OPERATIONS: usize = 4_096;

/// Collects operations and stops once the batch would exceed the operation limit.
#[derive(Default)]
pub(crate) struct Emitter {
    pub ops: Vec<Op>,
    pub overflowed: bool,
    /// Reproduce the draft's property order too, as the upstream tracker's recorded ops do.
    pub preserve_order: bool,
    /// String members a change appended to with `append_text()`, by path.
    pub appends: HashMap<Path, AppendHint>,
}

/// What `append_text()` appended to one string member: it holds while the base and draft strings are the very ones
/// it saw, so the batch records `text` without comparing the two strings.
#[derive(Clone)]
pub(crate) struct AppendHint {
    pub base: Arc<str>,
    pub after: Arc<str>,
    pub text: Arc<str>,
}

impl Emitter {
    pub fn emit(&mut self, op: Op) {
        if self.overflowed {
            return;
        }
        if self.ops.len() >= MAX_DELTA_OPERATIONS {
            self.overflowed = true;
            return;
        }
        self.ops.push(op);
    }

    pub fn emit_set(&mut self, path: &[Seg], value: &Json) {
        if path.is_empty() {
            self.emit(Op::Replace(value.clone()));
        } else {
            self.emit(Op::Set(path.to_vec(), value.clone()));
        }
    }
}

pub(crate) fn child(path: &[Seg], segment: Seg) -> Path {
    let mut next = Vec::with_capacity(path.len() + 1);
    next.extend_from_slice(path);
    next.push(segment);
    next
}

/// Emit the cheapest string edit: an append, a front truncation plus append, or a set.
pub(crate) fn emit_string(before: &str, after: &str, path: &[Seg], out: &mut Emitter) {
    if before == after {
        return;
    }
    if after.len() > before.len() && after.starts_with(before) {
        out.emit(Op::Append(path.to_vec(), after[before.len()..].into()));
        return;
    }
    let shared = overlap(before, after, DEFAULT_OVERLAP_SCAN);
    let offset = if shared == 0 { None } else { utf16_offset(after, shared) };
    let Some(offset) = offset else {
        out.emit(Op::Set(path.to_vec(), Json::from(after)));
        return;
    };
    out.emit(Op::Truncate(path.to_vec(), (utf16_len(before) - shared) as u64));
    if offset < after.len() {
        out.emit(Op::Append(path.to_vec(), after[offset..].into()));
    }
}

/// Keys of `after` that must be deleted and set again so the result has `after`'s key order.
fn moved_keys(before: &JsonMap, after: &JsonMap) -> Vec<Arc<str>> {
    let named = |map: &JsonMap| map.keys().filter(|key| array_index_key(key).is_none()).cloned().collect::<Vec<_>>();
    let (target, base) = (named(after), named(before));
    let kept: Vec<&Arc<str>> = base.iter().filter(|key| after.contains_key(key)).collect();
    let mut position = 0;
    let mut prefix = 0;
    for key in &target {
        match kept[position..].iter().position(|candidate| *candidate == key) {
            Some(offset) if before.contains_key(key) => {
                position += offset + 1;
                prefix += 1;
            }
            _ => break,
        }
    }
    let skipped = kept.len() - prefix;
    if skipped == 0 {
        return Vec::new();
    }
    target[prefix..].iter().filter(|key| before.contains_key(key)).cloned().collect()
}

fn diff_object(before: &JsonMap, after: &JsonMap, path: &[Seg], out: &mut Emitter, after_value: &Json) {
    if before.keys().chain(after.keys()).any(|key| is_reserved(key)) {
        if before != after {
            out.emit_set(path, after_value);
        }
        return;
    }
    let moved = if out.preserve_order { moved_keys(before, after) } else { Vec::new() };
    for (key, value) in after.iter() {
        if out.overflowed {
            return;
        }
        let next = child(path, Seg::Key(key.clone()));
        if moved.contains(key) {
            out.emit(Op::Delete(next.clone()));
            out.emit_set(&next, value);
            continue;
        }
        match before.get(key) {
            Some(previous) => diff_value(previous, value, &next, out),
            None => out.emit_set(&next, value),
        }
    }
    for key in before.keys() {
        if out.overflowed {
            return;
        }
        if !after.contains_key(key) {
            out.emit(Op::Delete(child(path, Seg::Key(Arc::clone(key)))));
        }
    }
}

pub(crate) fn diff_value(before: &Json, after: &Json, path: &[Seg], out: &mut Emitter) {
    if before.same_ref(after) || out.overflowed {
        return;
    }
    match (before, after) {
        (Json::String(left), Json::String(right)) if !path.is_empty() => {
            match out.appends.get(path).filter(|hint| Arc::ptr_eq(&hint.base, left) && Arc::ptr_eq(&hint.after, right))
            {
                Some(hint) => {
                    let text = hint.text.clone();
                    out.emit(Op::Append(path.to_vec(), text));
                }
                None => emit_string(left, right, path, out),
            }
        }
        (Json::Array(left), Json::Array(right)) => diff_array(left, right, path, out),
        (Json::Object(left), Json::Object(right)) => diff_object(left, right, path, out, after),
        _ => out.emit_set(path, after),
    }
}

pub(crate) fn json_cost(value: &Json) -> usize {
    match value {
        Json::Null => 4,
        Json::String(text) => utf16_len(text) + 2,
        Json::Number(number) => format_js_number(number.as_f64()).len(),
        Json::Bool(value) => {
            if *value {
                4
            } else {
                5
            }
        }
        Json::Array(items) => {
            2 + items.iter().enumerate().map(|(index, item)| json_cost(item) + usize::from(index > 0)).sum::<usize>()
        }
        Json::Object(map) => {
            2 + map
                .iter()
                .enumerate()
                .map(|(index, (key, item))| utf16_len(key) + 3 + json_cost(item) + usize::from(index > 0))
                .sum::<usize>()
        }
    }
}

fn path_cost(path: &[Seg]) -> usize {
    let segments: usize = path
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            let cost = match segment {
                Seg::Key(key) => utf16_len(key) + 2,
                Seg::Index(value) => value.to_string().len(),
            };
            cost + usize::from(index > 0)
        })
        .sum();
    2 + segments
}

fn digits(value: u64) -> usize {
    value.to_string().len()
}

fn operation_cost(op: &Op) -> usize {
    match op {
        Op::Replace(value) => 6 + json_cost(value),
        Op::Set(path, value) => 7 + path_cost(path) + json_cost(value),
        Op::Delete(path) => 6 + path_cost(path),
        Op::Append(path, text) => 7 + path_cost(path) + utf16_len(text) + 2,
        Op::Truncate(path, count) => 7 + path_cost(path) + digits(*count),
        Op::Splice(path, index, remove, items) => {
            10 + path_cost(path)
                + digits(*index)
                + digits(*remove)
                + 2
                + items.iter().enumerate().map(|(i, item)| json_cost(item) + usize::from(i > 0)).sum::<usize>()
        }
        Op::Move(path, permutation) => {
            7 + path_cost(path)
                + 2
                + permutation
                    .iter()
                    .enumerate()
                    .map(|(i, item)| digits(*item as u64) + usize::from(i > 0))
                    .sum::<usize>()
        }
    }
}

/// Draft batches: like the tracker, they fold to a root replacement only past the operation limit.
pub(crate) fn diff_draft(before: &Json, after: &Json) -> Vec<Op> {
    diff_with(before, after, false, HashMap::new())
}

/// The tracker's batch: like [`diff_draft`], keeping the draft's property order, with the change's appends.
pub(crate) fn diff_draft_ordered(before: &Json, after: &Json, appends: HashMap<Path, AppendHint>) -> Vec<Op> {
    diff_with(before, after, true, appends)
}

fn diff_with(before: &Json, after: &Json, preserve_order: bool, appends: HashMap<Path, AppendHint>) -> Vec<Op> {
    let mut out = Emitter { preserve_order, appends, ..Emitter::default() };
    diff_value(before, after, &[], &mut out);
    if out.overflowed { vec![Op::Replace(after.clone())] } else { out.ops }
}

/// Compute a compact operation batch from two immutable JSON revisions.
pub fn diff_revisions(before: &Json, after: &Json) -> Vec<Op> {
    let ops = diff_draft(before, after);
    if ops.is_empty() || ops[0].is_replace() {
        return ops;
    }
    let delta_cost = 2 + ops.iter().map(|op| operation_cost(op) + 1).sum::<usize>();
    if delta_cost < 65_536 {
        return ops;
    }
    let snapshot_cost = json_cost(after) + 6;
    if delta_cost >= snapshot_cost { vec![Op::Replace(after.clone())] } else { ops }
}
