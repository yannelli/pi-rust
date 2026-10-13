//! Source map v3 output for bundles: line-start and edit-boundary mappings.

use crate::json::{Json, utf16_len};

/// Byte offsets of line starts, for turning offsets into 0-based lines and UTF-16 columns.
pub(crate) struct LineIndex<'t> {
    text: &'t str,
    starts: Vec<usize>,
}

impl<'t> LineIndex<'t> {
    pub fn new(text: &'t str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        Self { text, starts }
    }

    pub fn locate(&self, offset: usize) -> (u32, u32) {
        let line = self.starts.partition_point(|start| *start <= offset) - 1;
        let start = self.starts[line];
        (line as u32, utf16_len(&self.text[start..offset.min(self.text.len())]) as u32)
    }
}

#[derive(Default)]
pub(crate) struct SourceMapBuilder {
    sources: Vec<String>,
    contents: Vec<String>,
    segments: Vec<[u32; 5]>,
}

fn vlq(out: &mut String, value: i64) {
    const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rest = if value < 0 { ((-value) << 1) | 1 } else { value << 1 };
    loop {
        let mut digit = rest & 31;
        rest >>= 5;
        if rest > 0 {
            digit |= 32;
        }
        out.push(DIGITS[digit as usize] as char);
        if rest == 0 {
            break;
        }
    }
}

impl SourceMapBuilder {
    pub fn add_source(&mut self, path: String, content: String) -> u32 {
        self.sources.push(path);
        self.contents.push(content);
        (self.sources.len() - 1) as u32
    }

    /// Map a generated position to a source position, all 0-based.
    pub fn add(&mut self, generated: (u32, u32), source: u32, original: (u32, u32)) {
        self.segments.push([generated.0, generated.1, source, original.0, original.1]);
    }

    fn mappings(&mut self) -> String {
        self.segments.sort_unstable();
        self.segments.dedup_by(|a, b| a[0] == b[0] && a[1] == b[1]);
        let mut out = String::new();
        let (mut line, mut column) = (0u32, 0i64);
        let (mut source, mut source_line, mut source_column) = (0i64, 0i64, 0i64);
        let mut first = true;
        for [generated_line, generated_column, index, original_line, original_column] in self.segments.iter().copied() {
            while line < generated_line {
                out.push(';');
                line += 1;
                column = 0;
                first = true;
            }
            if !first {
                out.push(',');
            }
            first = false;
            vlq(&mut out, generated_column as i64 - column);
            vlq(&mut out, index as i64 - source);
            vlq(&mut out, original_line as i64 - source_line);
            vlq(&mut out, original_column as i64 - source_column);
            column = generated_column as i64;
            (source, source_line, source_column) = (index as i64, original_line as i64, original_column as i64);
        }
        out
    }

    /// The source map JSON text, in esbuild's field order.
    pub fn finish(mut self) -> String {
        let mappings = self.mappings();
        let strings =
            |items: &[String]| Json::from(items.iter().map(|item| Json::from(item.as_str())).collect::<Vec<_>>());
        let mut map = crate::json::JsonMap::new();
        map.insert("version", 3);
        map.insert("sources", strings(&self.sources));
        map.insert("sourcesContent", strings(&self.contents));
        map.insert("mappings", mappings);
        map.insert("names", Json::from(Vec::<Json>::new()));
        format!("{}\n", Json::from(map).to_string_pretty(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_relative_segments() {
        let mut builder = SourceMapBuilder::default();
        let source = builder.add_source("a.ts".into(), "x".into());
        builder.add((0, 0), source, (0, 0));
        builder.add((0, 5), source, (1, 2));
        builder.add((2, 1), source, (0, 0));
        assert_eq!(builder.mappings(), "AAAA,KACE;;CADF");
        let index = LineIndex::new("ab\nc\u{e9}d");
        assert_eq!(index.locate(6), (1, 2));
    }
}
