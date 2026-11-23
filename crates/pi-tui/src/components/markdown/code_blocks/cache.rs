//! [`CodeBlockCache`]: top-level code blocks of streamed sources, lexed again only from the last
//! token that later text cannot change.

use super::{MarkdownCodeBlock, SpanKind, lex_spans};

/// Most scans a [`CodeBlockCache`] keeps.
const CACHED_SCANS: usize = 8;
/// Most bytes the kept scans hold; a larger scan is returned without being kept.
const CACHED_BYTES: usize = 16 << 20;

/// [`top_level_code_blocks`] with the scans of recent sources kept. A source that shares a prefix
/// with a cached one is lexed from the last resume point inside that prefix.
#[derive(Debug)]
pub struct CodeBlockCache {
    /// Least recently used first.
    scans: Vec<Scan>,
    byte_limit: usize,
    #[cfg(test)]
    lexed_bytes: usize,
}

impl Default for CodeBlockCache {
    fn default() -> Self {
        CodeBlockCache {
            scans: Vec::new(),
            byte_limit: CACHED_BYTES,
            #[cfg(test)]
            lexed_bytes: 0,
        }
    }
}

impl CodeBlockCache {
    pub fn top_level_code_blocks(&mut self, source: &str) -> Vec<MarkdownCodeBlock> {
        if let Some(index) = self.scans.iter().position(|scan| scan.source == source) {
            let scan = self.scans.remove(index);
            let blocks = scan.blocks.clone();
            self.scans.push(scan);
            return blocks;
        }
        let resume = self
            .scans
            .iter()
            .enumerate()
            .filter_map(|(index, scan)| Some((index, scan.resume_point(source)?)))
            .max_by_key(|&(_, start)| start);
        let scan = match resume {
            Some((index, start)) => {
                #[cfg(test)]
                {
                    self.lexed_bytes += source.len() - start;
                }
                let previous = if source.starts_with(self.scans[index].source.as_str()) {
                    self.scans.remove(index)
                } else {
                    self.scans[index].clone()
                };
                previous.resumed(source, start)
            }
            None => {
                #[cfg(test)]
                {
                    self.lexed_bytes += source.len();
                }
                Scan::new(source)
            }
        };
        if scan.retained_bytes() > self.byte_limit {
            return scan.blocks;
        }
        let blocks = scan.blocks.clone();
        self.scans.push(scan);
        let mut bytes: usize = self.scans.iter().map(Scan::retained_bytes).sum();
        while self.scans.len() > CACHED_SCANS || bytes > self.byte_limit {
            bytes -= self.scans.remove(0).retained_bytes();
        }
        blocks
    }
}

/// A source byte offset where lexing can start again, and the end of the line it starts.
#[derive(Debug, Clone, Copy)]
struct ResumePoint {
    start: usize,
    line_end: usize,
}

/// One source with its code blocks and the token starts a longer source can lex again from.
/// See `crates/pi-tui/docs/marked-code-blocks.md` for why the resume points are stable.
#[derive(Debug, Clone)]
struct Scan {
    source: String,
    blocks: Vec<MarkdownCodeBlock>,
    resume_points: Vec<ResumePoint>,
    /// Starts of lines outside code blocks that open with up to three spaces and `[`.
    label_lines: Vec<usize>,
}

impl Scan {
    fn retained_bytes(&self) -> usize {
        let strings: usize = self.blocks.iter().map(|block| block.lang.capacity() + block.text.capacity()).sum();
        self.source.capacity()
            + strings
            + self.blocks.capacity() * size_of::<MarkdownCodeBlock>()
            + self.resume_points.capacity() * size_of::<ResumePoint>()
            + self.label_lines.capacity() * size_of::<usize>()
    }

    fn new(source: &str) -> Scan {
        let mut scan =
            Scan { source: source.to_string(), blocks: Vec::new(), resume_points: Vec::new(), label_lines: Vec::new() };
        scan.lex_from(0);
        scan
    }

    /// The latest resume point whose first line `source` repeats, before any line where `source`
    /// can open a link definition.
    fn resume_point(&self, source: &str) -> Option<usize> {
        let common = common_prefix_len(self.source.as_bytes(), source.as_bytes());
        let last = self.resume_points.iter().rev().find(|point| point.line_end <= common)?.start;
        let hazard = self
            .label_lines
            .iter()
            .copied()
            .take_while(|&line| line < last)
            .find(|&line| closes_definition_label(source.as_bytes(), line));
        match hazard {
            None => Some(last),
            Some(line) => self.resume_points.iter().rev().find(|point| point.start <= line).map(|point| point.start),
        }
    }

    /// This scan for `source`, which repeats the first line of the resume point `start`.
    fn resumed(mut self, source: &str, start: usize) -> Scan {
        self.blocks.truncate(self.blocks.partition_point(|block| block.range.start < start));
        self.resume_points.truncate(self.resume_points.partition_point(|point| point.start <= start));
        self.label_lines.truncate(self.label_lines.partition_point(|&line| line < start));
        self.source.clear();
        self.source.push_str(source);
        self.lex_from(start);
        self
    }

    fn lex_from(&mut self, start: usize) {
        let (blocks, spans) = lex_spans(&self.source, start);
        for pair in spans.windows(2) {
            let ((before, _), (kind, range)) = (&pair[0], &pair[1]);
            let resumable = matches!(kind, SpanKind::Fence | SpanKind::AtxHeading)
                || (*kind != SpanKind::Space
                    && matches!(before, SpanKind::Space | SpanKind::Fence | SpanKind::AtxHeading));
            if let Some(line_end) = line_end(self.source.as_bytes(), range.start).filter(|_| resumable) {
                self.resume_points.push(ResumePoint { start: range.start, line_end });
            }
        }
        self.label_lines.extend(label_lines(self.source.as_bytes(), start, &blocks));
        self.blocks.extend(blocks);
    }
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    const CHUNK: usize = 256;
    let len = a.len().min(b.len());
    let mut equal = 0;
    while equal + CHUNK <= len && a[equal..equal + CHUNK] == b[equal..equal + CHUNK] {
        equal += CHUNK;
    }
    equal + a[equal..len].iter().zip(&b[equal..len]).take_while(|(a, b)| a == b).count()
}

/// The offset after the line terminator of the line at `start`; `None` for an unterminated line.
fn line_end(source: &[u8], start: usize) -> Option<usize> {
    source[start..].iter().position(|&byte| byte == b'\n' || byte == b'\r').map(|index| start + index + 1)
}

/// Starts of the lines from `start` on that open with up to three spaces and `[`, outside `blocks`.
fn label_lines(source: &[u8], start: usize, blocks: &[MarkdownCodeBlock]) -> Vec<usize> {
    let mut lines = Vec::new();
    let mut blocks = blocks.iter().peekable();
    let mut line = Some(start);
    while let Some(start) = line {
        while blocks.next_if(|block| block.range.end <= start).is_some() {}
        let in_block = blocks.peek().is_some_and(|block| block.range.start < start);
        let indent = source[start..].iter().take(4).take_while(|&&byte| byte == b' ').count();
        if !in_block && indent <= 3 && source.get(start + indent) == Some(&b'[') {
            lines.push(start);
        }
        line = line_end(source, start).map(|end| match (source[end - 1], source.get(end)) {
            (b'\r', Some(b'\n')) => end + 1,
            _ => end,
        });
    }
    lines
}

/// Whether the bracket that opens the line at `line` closes with `]:`. A definition label ends at
/// the first unescaped bracket.
fn closes_definition_label(source: &[u8], line: usize) -> bool {
    let mut index = line + source[line..].iter().position(|&byte| byte == b'[').unwrap_or(0) + 1;
    while let Some(&byte) = source.get(index) {
        match byte {
            b'\\' => index += 2,
            b'[' => return false,
            b']' => return source.get(index + 1) == Some(&b':'),
            _ => index += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests;
