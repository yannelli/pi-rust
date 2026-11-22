//! Token diff ported from jsdiff 8.0.4 (BSD-3-Clause, `LICENSE-jsdiff`), the `diff` package upstream pi uses. It
//! keeps the greedy Myers search and tie-breaks of jsdiff, so diffs and patches match upstream.

mod lines;
#[cfg(test)]
mod tests;
mod words;

pub use lines::{
    DEFAULT_CONTEXT_LINES, EditDiff, generate_diff_string, generate_edit_diff, generate_unified_patch, split_lines,
};
pub use words::{diff_words, leading_whitespace};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Equal,
    Removed,
    Added,
}

/// A run of `len` tokens from `start`. `Removed` runs index the old tokens; the others index the new tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub start: usize,
    pub len: usize,
}

const NONE: u32 = u32::MAX;
const MIN_COMPACT_LEN: usize = 1 << 16;
/// `old_used` of a diagonal without a path. The two path checks of the search fail for it.
const NO_PATH: i32 = i32::MAX / 2;
/// Positions are `i32`. jsdiff cannot hold inputs this large either.
const MAX_TOKENS: usize = (i32::MAX / 4) as usize;

const KIND_BITS: u32 = 2;
const KIND_MASK: u32 = (1 << KIND_BITS) - 1;

/// A run of tokens of one kind. `run` is the token count above the `KIND_BITS` of the kind.
#[derive(Clone, Copy)]
struct Component {
    run: u32,
    prev: u32,
}

impl Component {
    fn kind(self) -> ChangeKind {
        match self.run & KIND_MASK {
            0 => ChangeKind::Equal,
            1 => ChangeKind::Removed,
            _ => ChangeKind::Added,
        }
    }

    fn count(self) -> u32 {
        self.run >> KIND_BITS
    }
}

/// A search path. Its last run is not in the arena, so one more token of the same kind adds no component.
#[derive(Clone, Copy)]
struct Path {
    /// The old tokens that the path has used: jsdiff `oldPos + 1`.
    old_used: i32,
    last: Component,
}

const EMPTY: Path = Path { old_used: NO_PATH, last: Component { run: 0, prev: NONE } };

/// Linked lists of components that search paths share, like the change objects of jsdiff.
struct Components {
    nodes: Vec<Component>,
    remap: Vec<u32>,
    compact_at: usize,
}

impl Components {
    /// jsdiff `addToPath`, and the component that `extractCommon` adds.
    fn add(&mut self, path: &mut Path, kind: ChangeKind, count: u32) {
        if path.last.run & KIND_MASK != kind as u32 {
            if path.last.count() > 0 {
                path.last.prev = self.push(path.last);
            }
            path.last.run = kind as u32;
        }
        path.last.run += count << KIND_BITS;
    }

    #[cold]
    #[inline(never)]
    fn push(&mut self, component: Component) -> u32 {
        self.nodes.push(component);
        (self.nodes.len() - 1) as u32
    }

    /// Drops the components that no stored path reaches. A component only links to older ones.
    fn compact(&mut self, paths: &mut [Path]) {
        if self.nodes.len() < self.compact_at {
            return;
        }
        let remap = &mut self.remap;
        remap.clear();
        remap.resize(self.nodes.len(), NONE);
        for path in paths.iter().filter(|path| path.old_used != NO_PATH) {
            let mut i = path.last.prev;
            while i != NONE && remap[i as usize] == NONE {
                remap[i as usize] = 0;
                i = self.nodes[i as usize].prev;
            }
        }
        let mut kept = 0;
        for i in 0..self.nodes.len() {
            if remap[i] == NONE {
                continue;
            }
            let mut node = self.nodes[i];
            if node.prev != NONE {
                node.prev = remap[node.prev as usize];
            }
            self.nodes[kept] = node;
            remap[i] = kept as u32;
            kept += 1;
        }
        self.nodes.truncate(kept);
        for path in paths.iter_mut().filter(|path| path.old_used != NO_PATH && path.last.prev != NONE) {
            path.last.prev = remap[path.last.prev as usize];
        }
        self.compact_at = MIN_COMPACT_LEN.max(kept * 4);
    }

    fn changes(&self, last: Component) -> Vec<Change> {
        let mut runs = Vec::new();
        let mut next = Some(last).filter(|last| last.count() > 0);
        while let Some(component) = next {
            runs.push(component);
            next = self.nodes.get(component.prev as usize).copied();
        }
        let (mut old_pos, mut new_pos) = (0, 0);
        runs.iter()
            .rev()
            .map(|run| {
                let (kind, len) = (run.kind(), run.count() as usize);
                let start = if kind == ChangeKind::Removed { old_pos } else { new_pos };
                if kind != ChangeKind::Added {
                    old_pos += len;
                }
                if kind != ChangeKind::Removed {
                    new_pos += len;
                }
                Change { kind, start, len }
            })
            .collect()
    }
}

/// jsdiff `Diff.diff` on tokens: the changes that turn `old` into `new`.
pub fn diff_tokens<T>(old: &[T], new: &[T], equals: impl Fn(&T, &T) -> bool) -> Vec<Change> {
    let changes = search(old.len(), new.len(), usize::MAX, &ByIndex(|i: usize, j: usize| equals(&old[i], &new[j])));
    changes.unwrap_or_else(|| replace_all(old.len(), new.len()))
}

/// The tokens of a search. The search names each token by its index.
trait Tokens {
    fn equals(&self, old: usize, new: usize) -> bool;

    /// The count of equal tokens from `old` and `new`. Each text has `max` or more tokens from there.
    fn common_len(&self, old: usize, new: usize, max: usize) -> usize {
        (0..max).take_while(|&i| self.equals(old + i, new + i)).count()
    }
}

struct ByIndex<F>(F);

impl<F: Fn(usize, usize) -> bool> Tokens for ByIndex<F> {
    fn equals(&self, old: usize, new: usize) -> bool {
        (self.0)(old, new)
    }
}

/// The changes for tokens that have no match: jsdiff removes the old ones, then adds the new ones.
fn replace_all(old_tokens: usize, new_tokens: usize) -> Vec<Change> {
    [(ChangeKind::Removed, old_tokens), (ChangeKind::Added, new_tokens)]
        .into_iter()
        .filter(|&(_, len)| len > 0)
        .map(|(kind, len)| Change { kind, start: 0, len })
        .collect()
}

/// jsdiff `extractCommon`: the count of equal tokens from `old_used` and `new_used`.
#[inline(never)]
fn common_len(tokens: &impl Tokens, (old_used, new_used): (i32, i32), (old_len, new_len): (i32, i32)) -> i32 {
    let max = (old_len - old_used).min(new_len - new_used);
    tokens.common_len(old_used as usize, new_used as usize, max as usize) as i32
}

/// jsdiff `Diff.diff`. Returns `None` when the changes need more than `max_edit_length` added
/// and removed tokens.
fn search(old_tokens: usize, new_tokens: usize, max_edit_length: usize, tokens: &impl Tokens) -> Option<Vec<Change>> {
    assert!(old_tokens.max(new_tokens) <= MAX_TOKENS, "too many tokens to diff");
    let (old_len, new_len) = (old_tokens as i32, new_tokens as i32);
    let mut components = Components { nodes: Vec::new(), remap: Vec::new(), compact_at: MIN_COMPACT_LEN };
    let common = common_len(tokens, (0, 0), (old_len, new_len));
    let mut first = Path { old_used: common, ..EMPTY };
    if common > 0 {
        components.add(&mut first, ChangeKind::Equal, common as u32);
    }
    if common >= old_len && common >= new_len {
        return Some(components.changes(first.last));
    }
    // Paths by diagonal `old_used - new_used`. They only reach `-new_len..=old_len`, so the
    // search skips the diagonals past those bounds, where jsdiff finds no path.
    let offset = new_len + 2;
    let mut best: Vec<Path> = vec![EMPTY; old_tokens + new_tokens + 5];
    best[offset as usize] = first;
    let (mut min_diagonal, mut max_diagonal) = (i32::MIN, i32::MAX);
    let last_edit_length = (old_len + new_len).min(i32::try_from(max_edit_length).unwrap_or(i32::MAX));
    for edit_length in 1..=last_edit_length {
        let mut diagonal = min_diagonal.max(-edit_length);
        if diagonal < -new_len - 1 {
            diagonal += (-new_len - diagonal) / 2 * 2;
        }
        let mut last_diagonal = max_diagonal.min(edit_length).min(old_len + 1);
        while diagonal <= last_diagonal {
            let i = (diagonal + offset) as usize;
            let (remove, add) = (best[i - 1], best[i + 1]);
            best[i - 1].old_used = NO_PATH;
            let can_remove = remove.old_used < old_len;
            // The add path is on the diagonal above, so it has used `old_used - diagonal - 1` new tokens.
            let can_add = add.old_used - diagonal <= new_len;
            let (mut path, kind) = if can_add && (!can_remove || remove.old_used < add.old_used) {
                (add, ChangeKind::Added)
            } else if can_remove {
                (Path { old_used: remove.old_used + 1, ..remove }, ChangeKind::Removed)
            } else {
                best[i].old_used = NO_PATH;
                diagonal += 2;
                continue;
            };
            components.add(&mut path, kind, 1);
            let mut new_used = path.old_used - diagonal;
            // Most paths stop at once, so the loop that follows a match is not in this loop.
            if path.old_used < old_len && new_used < new_len && tokens.equals(path.old_used as usize, new_used as usize)
            {
                let common = 1 + common_len(tokens, (path.old_used + 1, new_used + 1), (old_len, new_len));
                components.add(&mut path, ChangeKind::Equal, common as u32);
                path.old_used += common;
                new_used += common;
            }
            best[i] = path;
            if path.old_used >= old_len || new_used >= new_len {
                if path.old_used >= old_len && new_used >= new_len {
                    return Some(components.changes(path.last));
                }
                // jsdiff stops the round at a path that has used all old tokens.
                if path.old_used >= old_len {
                    max_diagonal = max_diagonal.min(diagonal - 1);
                    last_diagonal = last_diagonal.min(diagonal - 1);
                }
                if new_used >= new_len {
                    min_diagonal = min_diagonal.max(diagonal + 1);
                }
            }
            diagonal += 2;
        }
        components.compact(&mut best);
    }
    None
}
