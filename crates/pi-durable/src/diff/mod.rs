//! Token diff ported from jsdiff 8.0.4 (BSD-3-Clause, `LICENSE-jsdiff`), the `diff` package upstream pi uses. It
//! keeps the greedy Myers search and tie-breaks of jsdiff, so diffs and patches match upstream.

mod lines;
#[cfg(test)]
mod tests;

pub use lines::{EditDiff, generate_diff_string, generate_edit_diff, generate_unified_patch, split_lines};

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

#[derive(Clone, Copy)]
struct Component {
    kind: ChangeKind,
    count: u32,
    prev: u32,
}

#[derive(Clone, Copy)]
struct Path {
    old_pos: isize,
    last: u32,
}

/// Linked lists of components that search paths share, like the change objects of jsdiff.
struct Components {
    nodes: Vec<Component>,
    remap: Vec<u32>,
    compact_at: usize,
}

impl Components {
    fn push(&mut self, kind: ChangeKind, count: u32, prev: u32) -> u32 {
        self.nodes.push(Component { kind, count, prev });
        (self.nodes.len() - 1) as u32
    }

    fn get(&self, i: u32) -> Option<Component> {
        self.nodes.get(i as usize).copied()
    }

    /// jsdiff `addToPath`.
    fn extend(&mut self, path: Path, kind: ChangeKind) -> Path {
        let old_pos = path.old_pos + isize::from(kind == ChangeKind::Removed);
        let last = match self.get(path.last) {
            Some(last) if last.kind == kind => self.push(kind, last.count + 1, last.prev),
            _ => self.push(kind, 1, path.last),
        };
        Path { old_pos, last }
    }

    /// Drops the components that no stored path reaches. A component only links to older ones.
    fn compact(&mut self, paths: &mut [Option<Path>]) {
        if self.nodes.len() < self.compact_at {
            return;
        }
        let remap = &mut self.remap;
        remap.clear();
        remap.resize(self.nodes.len(), NONE);
        for path in paths.iter().flatten() {
            let mut i = path.last;
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
        for path in paths.iter_mut().flatten() {
            if path.last != NONE {
                path.last = remap[path.last as usize];
            }
        }
        self.compact_at = MIN_COMPACT_LEN.max(kept * 4);
    }

    fn changes(&self, mut last: u32) -> Vec<Change> {
        let mut runs = Vec::new();
        while let Some(component) = self.get(last) {
            runs.push(component);
            last = component.prev;
        }
        let (mut old_pos, mut new_pos) = (0, 0);
        runs.iter()
            .rev()
            .map(|run| {
                let start = if run.kind == ChangeKind::Removed { old_pos } else { new_pos };
                let len = run.count as usize;
                if run.kind != ChangeKind::Added {
                    old_pos += len;
                }
                if run.kind != ChangeKind::Removed {
                    new_pos += len;
                }
                Change { kind: run.kind, start, len }
            })
            .collect()
    }
}

/// jsdiff `Diff.diff` on tokens: the changes that turn `old` into `new`.
pub fn diff_tokens<T>(old: &[T], new: &[T], equals: impl Fn(&T, &T) -> bool) -> Vec<Change> {
    let (old_len, new_len) = (old.len() as isize, new.len() as isize);
    let mut components = Components { nodes: Vec::new(), remap: Vec::new(), compact_at: MIN_COMPACT_LEN };
    // Paths by diagonal `old_pos - new_pos`. They only reach `-new_len..=old_len`, so the
    // search skips the diagonals past those bounds, where jsdiff finds no path.
    let offset = new_len + 2;
    let mut best: Vec<Option<Path>> = vec![None; old.len() + new.len() + 5];
    // jsdiff `extractCommon`: follow the diagonal while the tokens match.
    let extract_common = |components: &mut Components, path: &mut Path, diagonal: isize| {
        let start = path.old_pos;
        let mut old_pos = start;
        let mut new_pos = old_pos - diagonal;
        while new_pos + 1 < new_len
            && old_pos + 1 < old_len
            && equals(&old[(old_pos + 1) as usize], &new[(new_pos + 1) as usize])
        {
            old_pos += 1;
            new_pos += 1;
        }
        if old_pos > start {
            path.last = components.push(ChangeKind::Equal, (old_pos - start) as u32, path.last);
        }
        path.old_pos = old_pos;
        new_pos
    };

    let mut first = Path { old_pos: -1, last: NONE };
    let new_pos = extract_common(&mut components, &mut first, 0);
    if first.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
        return components.changes(first.last);
    }
    best[offset as usize] = Some(first);
    let (mut min_diagonal, mut max_diagonal) = (isize::MIN, isize::MAX);
    for edit_length in 1..=old_len + new_len {
        let mut diagonal = min_diagonal.max(-edit_length);
        if diagonal < -new_len - 1 {
            diagonal += (-new_len - diagonal) / 2 * 2;
        }
        while diagonal <= max_diagonal.min(edit_length).min(old_len + 1) {
            let i = (diagonal + offset) as usize;
            let remove_path = best[i - 1].take().filter(|path| path.old_pos + 1 < old_len);
            let add_path = best[i + 1].filter(|path| (0..new_len).contains(&(path.old_pos - diagonal)));
            let mut path = match (remove_path, add_path) {
                (Some(remove), Some(add)) if remove.old_pos < add.old_pos => components.extend(add, ChangeKind::Added),
                (Some(remove), _) => components.extend(remove, ChangeKind::Removed),
                (None, Some(add)) => components.extend(add, ChangeKind::Added),
                (None, None) => {
                    best[i] = None;
                    diagonal += 2;
                    continue;
                }
            };
            let new_pos = extract_common(&mut components, &mut path, diagonal);
            if path.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return components.changes(path.last);
            }
            best[i] = Some(path);
            if path.old_pos + 1 >= old_len {
                max_diagonal = max_diagonal.min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                min_diagonal = min_diagonal.max(diagonal + 1);
            }
            diagonal += 2;
        }
        components.compact(&mut best);
    }
    // Not reached: a path reaches the end within `old_len + new_len` edits.
    [(ChangeKind::Removed, old.len()), (ChangeKind::Added, new.len())]
        .into_iter()
        .filter(|&(_, len)| len > 0)
        .map(|(kind, len)| Change { kind, start: 0, len })
        .collect()
}
