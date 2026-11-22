//! The line tokens of jsdiff `diffLines` and the search for their changes.

use std::collections::HashMap;

use super::{ByIndex, Change, ChangeKind, Tokens, replace_all, search};

/// The line tokens of a text: line `i` is `text[bounds[i]..bounds[i + 1]]`.
pub(super) struct Lines<'a> {
    text: &'a str,
    bounds: Vec<usize>,
}

impl<'a> Lines<'a> {
    /// `known` holds the bounds of the first lines; the search for `\n` starts at the last of them.
    pub(super) fn new(text: &'a str, known: &[usize]) -> Self {
        const LOW_BITS: u64 = 0x7f7f_7f7f_7f7f_7f7f;
        const NEWLINES: u64 = 0x0a0a_0a0a_0a0a_0a0a;
        let mut bounds = known.to_vec();
        let from = known.last().copied().unwrap_or(0);
        let mut chunks = text.as_bytes()[from..].chunks_exact(8);
        for (i, chunk) in chunks.by_ref().enumerate() {
            let word = u64::from_le_bytes(chunk.try_into().unwrap_or_default()) ^ NEWLINES;
            // The high bit of each byte of `word` that is zero, so of each `\n`.
            let mut newlines = !(((word & LOW_BITS) + LOW_BITS) | word | LOW_BITS);
            while newlines != 0 {
                bounds.push(from + i * 8 + newlines.trailing_zeros() as usize / 8 + 1);
                newlines &= newlines - 1;
            }
        }
        let tail = text.len() - chunks.remainder().len();
        bounds.extend(
            chunks.remainder().iter().enumerate().filter(|&(_, &byte)| byte == b'\n').map(|(i, _)| tail + i + 1),
        );
        if bounds.last() != Some(&text.len()) {
            bounds.push(text.len());
        }
        Self { text, bounds }
    }

    pub(super) fn len(&self) -> usize {
        self.bounds.len() - 1
    }

    pub(super) fn get(&self, i: usize) -> &'a str {
        &self.text[self.bounds[i]..self.bounds[i + 1]]
    }

    fn bytes(&self, i: usize) -> &'a [u8] {
        &self.text.as_bytes()[self.bounds[i]..self.bounds[i + 1]]
    }

    /// `text.split('\n').count()`.
    pub(super) fn count(&self) -> usize {
        self.len() + usize::from(self.text.is_empty() || self.text.ends_with('\n'))
    }
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let blocks = a.chunks_exact(32).zip(b.chunks_exact(32)).take_while(|(x, y)| x == y).count() * 32;
    blocks + a[blocks..].iter().zip(&b[blocks..]).take_while(|(x, y)| x == y).count()
}

/// The lines from `from` of two texts, compared by their bytes.
struct ByBytes<'a, 'b> {
    old: &'b Lines<'a>,
    new: &'b Lines<'a>,
    from: usize,
}

impl Tokens for ByBytes<'_, '_> {
    fn equals(&self, old: usize, new: usize) -> bool {
        self.old.bytes(self.from + old) == self.new.bytes(self.from + new)
    }

    /// Compares the bytes of the two texts in one pass, then counts the lines in the equal part.
    fn common_len(&self, old: usize, new: usize, _max: usize) -> usize {
        let (old_start, new_start) = (self.old.bounds[self.from + old], self.new.bounds[self.from + new]);
        let bytes = common_prefix_len(&self.old.text.as_bytes()[old_start..], &self.new.text.as_bytes()[new_start..]);
        let ends = &self.old.bounds[self.from + old..];
        let lines = ends.partition_point(|&end| end <= old_start + bytes) - 1;
        // A last line without `\n` is only equal to the last line of the other text.
        let ends_both =
            lines == 0 || self.old.text[..ends[lines]].ends_with('\n') || new_start + bytes == self.new.text.len();
        lines - usize::from(!ends_both)
    }
}

/// An id for each line from `from`, the same for lines with equal bytes, and the count of ids
/// of `old`. The ids of `old` come first, so a line of `new` with a smaller id is also in `old`.
fn intern(old: &Lines, new: &Lines, from: usize) -> (Vec<u32>, Vec<u32>, u32) {
    // The std hasher has a random key, so lines from a file cannot be made to collide.
    let mut ids = HashMap::with_capacity(old.len() + new.len() - 2 * from);
    let mut id_of = |line| {
        let next = ids.len() as u32;
        *ids.entry(line).or_insert(next)
    };
    let old_ids: Vec<u32> = (from..old.len()).map(|i| id_of(old.bytes(i))).collect();
    let old_distinct = old_ids.iter().max().map_or(0, |id| id + 1);
    (old_ids, (from..new.len()).map(|i| id_of(new.bytes(i))).collect(), old_distinct)
}

/// The lines of both texts and the changes between them.
pub(super) fn line_changes<'a>(old_text: &'a str, new_text: &'a str) -> (Lines<'a>, Lines<'a>, Vec<Change>) {
    let old = Lines::new(old_text, &[0]);
    // The lines with `\n` in the common byte prefix are the same tokens in both texts, so
    // the search starts after them. jsdiff follows them in its first `extractCommon`.
    let common = common_prefix_len(old_text.as_bytes(), new_text.as_bytes());
    let mut shared = old.bounds.partition_point(|&bound| bound <= common) - 1;
    if shared > 0 && !old.bytes(shared - 1).ends_with(b"\n") {
        shared -= 1;
    }
    let new = Lines::new(new_text, &old.bounds[..=shared]);
    let (old_rest, new_rest) = (old.len() - shared, new.len() - shared);
    // A search with few edits compares the bytes of the lines. A longer one starts again with line ids.
    let max_edit_length = 16 + 2 * (old_rest + new_rest).isqrt();
    let by_bytes = ByBytes { old: &old, new: &new, from: shared };
    let mut changes = search(old_rest, new_rest, max_edit_length, &by_bytes).unwrap_or_else(|| {
        let (old_ids, new_ids, old_distinct) = intern(&old, &new, shared);
        // With no line in both texts the search has one result.
        let found = match new_ids.iter().any(|&id| id < old_distinct) {
            true => search(old_rest, new_rest, usize::MAX, &ByIndex(|i: usize, j: usize| old_ids[i] == new_ids[j])),
            false => None,
        };
        found.unwrap_or_else(|| replace_all(old_rest, new_rest))
    });
    if shared > 0 {
        changes.iter_mut().for_each(|change| change.start += shared);
        match changes.first_mut().filter(|first| first.kind == ChangeKind::Equal) {
            Some(first) => *first = Change { kind: ChangeKind::Equal, start: 0, len: first.len + shared },
            None => changes.insert(0, Change { kind: ChangeKind::Equal, start: 0, len: shared }),
        }
    }
    (old, new, changes)
}
