//! Rendered lines in shared chunks (see `Component::render_chunks`). Unchanged chunks keep their
//! `Rc`, so a frame of a long transcript joins chunks instead of copying every line again.

use std::rc::Rc;

/// Rendered lines as shared chunks. Joined in order, they are the lines of `Component::render`.
pub type LineChunks = Vec<Rc<Vec<String>>>;

thread_local! {
    static BLANK_LINE: Rc<Vec<String>> = Rc::new(vec![String::new()]);
}

/// A shared chunk with one empty line.
pub fn blank_line_chunk() -> Rc<Vec<String>> {
    BLANK_LINE.with(Rc::clone)
}

/// Total number of lines in `chunks`.
pub fn chunks_len(chunks: &[Rc<Vec<String>>]) -> usize {
    chunks.iter().map(|chunk| chunk.len()).sum()
}

/// Joins `chunks` into owned lines. Chunks that nothing else holds move into the result.
pub fn flatten_chunks(chunks: LineChunks) -> Vec<String> {
    let mut lines = Vec::with_capacity(chunks_len(&chunks));
    for chunk in chunks {
        match Rc::try_unwrap(chunk) {
            Ok(owned) => lines.extend(owned),
            Err(shared) => lines.extend(shared.iter().cloned()),
        }
    }
    lines
}

/// Whether `a` and `b` hold the same lines: the same `Rc`s, or else equal text.
pub fn same_lines(a: &[Rc<Vec<String>>], b: &[Rc<Vec<String>>]) -> bool {
    if a.len() == b.len() && a.iter().zip(b).all(|(a, b)| Rc::ptr_eq(a, b)) {
        return true;
    }
    chunks_len(a) == chunks_len(b)
        && a.iter().flat_map(|chunk| chunk.iter()).eq(b.iter().flat_map(|chunk| chunk.iter()))
}

/// The output of a pure function of rendered chunks, computed again only when the input lines change.
#[derive(Default)]
pub struct ChunkMemo {
    input: LineChunks,
    output: Option<Rc<Vec<String>>>,
}

impl ChunkMemo {
    /// The memoized `transform(input)`. While the input lines stay the same, returns the same `Rc`.
    pub fn get(
        &mut self,
        input: LineChunks,
        transform: impl FnOnce(&[Rc<Vec<String>>]) -> Vec<String>,
    ) -> Rc<Vec<String>> {
        if let Some(output) = &self.output
            && same_lines(&self.input, &input)
        {
            let output = output.clone();
            self.input = input;
            return output;
        }
        let output = Rc::new(transform(&input));
        self.input = input;
        self.output = Some(output.clone());
        output
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.output = None;
    }
}

/// Lines kept in their rendered chunks, with indexed access (used by the fullscreen layout).
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ChunkedLines {
    chunks: LineChunks,
    /// Exclusive end line of each chunk.
    ends: Vec<usize>,
}

impl ChunkedLines {
    pub fn new(chunks: LineChunks) -> Self {
        let chunks: LineChunks = chunks.into_iter().filter(|chunk| !chunk.is_empty()).collect();
        let mut end = 0;
        let ends = chunks
            .iter()
            .map(|chunk| {
                end += chunk.len();
                end
            })
            .collect();
        Self { chunks, ends }
    }

    pub fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&String> {
        let chunk = self.ends.partition_point(|&end| end <= index);
        let start = if chunk == 0 { 0 } else { self.ends[chunk - 1] };
        self.chunks.get(chunk)?.get(index - start)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &String> {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }

    pub fn to_vec(&self) -> Vec<String> {
        self.iter().cloned().collect()
    }

    pub fn chunks(&self) -> &[Rc<Vec<String>>] {
        &self.chunks
    }
}

impl From<Vec<String>> for ChunkedLines {
    fn from(lines: Vec<String>) -> Self {
        Self::new(vec![Rc::new(lines)])
    }
}

impl std::ops::Index<usize> for ChunkedLines {
    type Output = String;

    fn index(&self, index: usize) -> &String {
        self.get(index).expect("line index out of range")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(lines: &[&str]) -> Rc<Vec<String>> {
        Rc::new(lines.iter().map(|line| line.to_string()).collect())
    }

    #[test]
    fn indexes_lines_across_chunks() {
        let lines = ChunkedLines::new(vec![chunk(&["a", "b"]), chunk(&[]), chunk(&["c"]), chunk(&["d", "e"])]);
        assert_eq!(lines.len(), 5);
        let all: Vec<&str> = (0..5).map(|index| lines[index].as_str()).collect();
        assert_eq!(all, ["a", "b", "c", "d", "e"]);
        assert_eq!(lines.get(5), None);
        assert_eq!(lines.to_vec(), ["a", "b", "c", "d", "e"]);
        assert!(ChunkedLines::default().get(0).is_none());
    }

    #[test]
    fn flatten_moves_unshared_chunks_and_copies_shared_ones() {
        let shared = chunk(&["kept"]);
        let lines = flatten_chunks(vec![shared.clone(), chunk(&["moved"])]);
        assert_eq!(lines, ["kept", "moved"]);
        assert_eq!(shared.as_slice(), ["kept"]);
    }

    #[test]
    fn memo_reuses_output_while_input_lines_are_equal() {
        let mut memo = ChunkMemo::default();
        let upper = |input: &[Rc<Vec<String>>]| input.iter().flat_map(|c| c.iter()).map(|l| l.to_uppercase()).collect();
        let first = memo.get(vec![chunk(&["a"]), chunk(&["b"])], upper);
        let same_text = memo.get(vec![chunk(&["a", "b"])], upper);
        assert!(Rc::ptr_eq(&first, &same_text));
        let changed = memo.get(vec![chunk(&["a", "c"])], upper);
        assert_eq!(changed.as_slice(), ["A", "C"]);
        memo.clear();
        assert!(!Rc::ptr_eq(&changed, &memo.get(vec![chunk(&["a", "c"])], upper)));
    }
}
