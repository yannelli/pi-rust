//! Array diffing: permutations, identity anchors, bounded LCS alignment, and region splices.

use std::collections::HashMap;

use super::diff::{Emitter, child, diff_value};
use super::{Op, Seg};
use crate::json::{Identity, Json};

const MAX_IDENTITY_CANDIDATES: usize = 200_000;
const MAX_SEMANTIC_CELLS: usize = 65_536;

type Match = (usize, usize);

fn same_value(left: &Json, right: &Json) -> bool {
    left == right
}

fn permutation(before: &[Json], after: &[Json]) -> Option<Vec<usize>> {
    if before.len() != after.len() {
        return None;
    }
    let mut positions: HashMap<Identity, (Vec<usize>, usize)> = HashMap::new();
    for (index, value) in before.iter().enumerate() {
        positions.entry(value.identity()).or_default().0.push(index);
    }
    let mut result = Vec::with_capacity(after.len());
    for value in after {
        let entry = positions.get_mut(&value.identity())?;
        let index = *entry.0.get(entry.1)?;
        entry.1 += 1;
        result.push(index);
    }
    Some(result)
}

fn lcs_matches(
    before: &[Json],
    after: &[Json],
    equal: fn(&Json, &Json) -> bool,
    max_cells: usize,
) -> Option<Vec<Match>> {
    if before.is_empty() || after.is_empty() {
        return Some(Vec::new());
    }
    if before.len() * after.len() > max_cells {
        return None;
    }
    let width = after.len() + 1;
    let mut lengths = vec![0u32; (before.len() + 1) * width];
    for left in (0..before.len()).rev() {
        for right in (0..after.len()).rev() {
            let at = left * width + right;
            lengths[at] = if equal(&before[left], &after[right]) {
                lengths[(left + 1) * width + right + 1] + 1
            } else {
                lengths[(left + 1) * width + right].max(lengths[left * width + right + 1])
            };
        }
    }
    let mut matches = Vec::new();
    let (mut left, mut right) = (0, 0);
    while left < before.len() && right < after.len() {
        if equal(&before[left], &after[right])
            && lengths[left * width + right] == lengths[(left + 1) * width + right + 1] + 1
        {
            matches.push((left, right));
            left += 1;
            right += 1;
        } else if lengths[(left + 1) * width + right] >= lengths[left * width + right + 1] {
            left += 1;
        } else {
            right += 1;
        }
    }
    Some(matches)
}

fn semantically_aligned(left: &Json, right: &Json) -> bool {
    if same_value(left, right) {
        return true;
    }
    match (left, right) {
        (Json::Array(left), Json::Array(right)) => {
            left.len() == right.len()
                && left.iter().zip(right.iter()).any(|(value, other)| value.is_container() && value.same_ref(other))
        }
        (Json::Object(left), Json::Object(right)) => left
            .iter()
            .any(|(key, value)| value.is_container() && right.get(key).is_some_and(|other| value.same_ref(other))),
        _ => false,
    }
}

fn lower_bound(values: &[usize], value: usize) -> usize {
    values.partition_point(|candidate| *candidate < value)
}

fn identity_subsequence(before: &[Json], b: (usize, usize), after: &[Json], a: (usize, usize)) -> Option<Vec<Match>> {
    let (before_start, before_end) = b;
    let (after_start, after_end) = a;
    let before_count = before_end - before_start;
    let after_count = after_end - after_start;
    let mut matches = Vec::new();
    if after_count < before_count {
        let mut before_index = before_start;
        for (after_index, value) in after.iter().enumerate().take(after_end).skip(after_start) {
            while before_index < before_end && !before[before_index].same_ref(value) {
                before_index += 1;
            }
            if before_index == before_end {
                return None;
            }
            matches.push((before_index, after_index));
            before_index += 1;
        }
        return Some(matches);
    }
    if before_count < after_count {
        let mut after_index = after_start;
        for (before_index, value) in before.iter().enumerate().take(before_end).skip(before_start) {
            while after_index < after_end && !value.same_ref(&after[after_index]) {
                after_index += 1;
            }
            if after_index == after_end {
                return None;
            }
            matches.push((before_index, after_index));
            after_index += 1;
        }
        return Some(matches);
    }
    None
}

fn greedy_identity_anchors(positions: &HashMap<Identity, Vec<usize>>, after: &[Json], a: (usize, usize)) -> Vec<Match> {
    let mut matches = Vec::new();
    let mut next = 0;
    for (after_index, value) in after.iter().enumerate().take(a.1).skip(a.0) {
        let Some(candidates) = positions.get(&value.identity()) else {
            continue;
        };
        let at = lower_bound(candidates, next);
        let Some(&before_index) = candidates.get(at) else {
            continue;
        };
        matches.push((before_index, after_index));
        next = before_index + 1;
    }
    matches
}

fn identity_anchors(before: &[Json], b: (usize, usize), after: &[Json], a: (usize, usize)) -> Vec<Match> {
    let mut positions: HashMap<Identity, Vec<usize>> = HashMap::new();
    for (index, value) in before.iter().enumerate().take(b.1).skip(b.0) {
        positions.entry(value.identity()).or_default().push(index);
    }
    let mut candidate_count = 0;
    for value in &after[a.0..a.1] {
        candidate_count += positions.get(&value.identity()).map_or(0, Vec::len);
        if candidate_count > MAX_IDENTITY_CANDIDATES {
            return greedy_identity_anchors(&positions, after, a);
        }
    }
    if candidate_count == 0 {
        return Vec::new();
    }
    // Longest increasing subsequence of before-indices over candidate pairs (patience sorting).
    let mut candidates: Vec<(usize, usize, Option<usize>)> = Vec::new();
    let mut tails: Vec<usize> = Vec::new();
    let mut tail_values: Vec<usize> = Vec::new();
    for (after_index, value) in after.iter().enumerate().take(a.1).skip(a.0) {
        let Some(before_positions) = positions.get(&value.identity()) else {
            continue;
        };
        for &before_index in before_positions.iter().rev() {
            let at = lower_bound(&tail_values, before_index);
            let candidate = candidates.len();
            candidates.push((before_index, after_index, if at == 0 { None } else { Some(tails[at - 1]) }));
            if at == tails.len() {
                tails.push(candidate);
                tail_values.push(before_index);
            } else {
                tails[at] = candidate;
                tail_values[at] = before_index;
            }
        }
    }
    let mut matches = Vec::new();
    let mut cursor = tails.last().copied();
    while let Some(index) = cursor {
        let (before_index, after_index, previous) = candidates[index];
        matches.push((before_index, after_index));
        cursor = previous;
    }
    matches.reverse();
    matches
}

struct Regions<'a> {
    before: &'a [Json],
    after: &'a [Json],
    path: &'a [Seg],
}

impl Regions<'_> {
    fn process(&self, out: &mut Emitter, b: (usize, usize), a: (usize, usize), output: usize, matches: &[Match]) {
        let (mut before_at, mut after_at, mut output_at) = (b.0, a.0, output);
        for &(before_match, after_match) in matches {
            if out.overflowed {
                return;
            }
            self.region(out, (before_at, before_match), (after_at, after_match), output_at);
            output_at += after_match - after_at;
            if !same_value(&self.before[before_match], &self.after[after_match]) {
                let next = child(self.path, Seg::Index(output_at as u64));
                diff_value(&self.before[before_match], &self.after[after_match], &next, out);
            }
            output_at += 1;
            before_at = before_match + 1;
            after_at = after_match + 1;
        }
        if !out.overflowed {
            self.region(out, (before_at, b.1), (after_at, a.1), output_at);
        }
    }

    fn splice(&self, out: &mut Emitter, output: usize, remove: usize, a: (usize, usize)) {
        out.emit(Op::Splice(self.path.to_vec(), output as u64, remove as u64, self.after[a.0..a.1].to_vec()));
    }

    fn region(&self, out: &mut Emitter, b: (usize, usize), a: (usize, usize), output: usize) {
        if out.overflowed {
            return;
        }
        let (before, after) = (self.before, self.after);
        let ((mut bs, mut be), (mut as_, mut ae), mut output) = (b, a, output);
        while bs < be && as_ < ae && same_value(&before[bs], &after[as_]) {
            bs += 1;
            as_ += 1;
            output += 1;
        }
        while bs < be && as_ < ae && same_value(&before[be - 1], &after[ae - 1]) {
            be -= 1;
            ae -= 1;
        }
        let before_count = be - bs;
        let after_count = ae - as_;
        if before_count == 0 && after_count == 0 {
            return;
        }
        if before_count == 0 || after_count == 0 {
            self.splice(out, output, before_count, (as_, ae));
            return;
        }
        let (b, a) = ((bs, be), (as_, ae));
        if before_count == after_count {
            let positional: Vec<Match> = (0..before_count)
                .filter(|offset| same_value(&before[bs + offset], &after[as_ + offset]))
                .map(|offset| (bs + offset, as_ + offset))
                .collect();
            if !positional.is_empty() {
                return self.process(out, b, a, output, &positional);
            }
        }
        if let Some(subsequence) = identity_subsequence(before, b, after, a).filter(|matches| !matches.is_empty()) {
            return self.process(out, b, a, output, &subsequence);
        }
        let identity = identity_anchors(before, b, after, a);
        if !identity.is_empty() {
            return self.process(out, b, a, output, &identity);
        }
        let semantic = lcs_matches(&before[bs..be], &after[as_..ae], semantically_aligned, MAX_SEMANTIC_CELLS);
        if let Some(semantic) = semantic.filter(|matches| !matches.is_empty()) {
            let absolute: Vec<Match> = semantic.iter().map(|&(left, right)| (bs + left, as_ + right)).collect();
            return self.process(out, b, a, output, &absolute);
        }
        if before_count == 1 && after_count == 1 {
            let next = child(self.path, Seg::Index(output as u64));
            diff_value(&before[bs], &after[as_], &next, out);
            return;
        }
        self.splice(out, output, before_count, a);
    }
}

pub(crate) fn diff_array(before: &[Json], after: &[Json], path: &[Seg], out: &mut Emitter) {
    if before == after {
        return;
    }
    let n = before.len();
    if n == after.len() && n > 1 && !same_value(&before[0], &after[0]) && !same_value(&before[n - 1], &after[n - 1]) {
        if let Some(order) = permutation(before, after) {
            out.emit(Op::Move(path.to_vec(), order));
            return;
        }
    }
    Regions { before, after, path }.region(out, (0, n), (0, after.len()), 0);
}
