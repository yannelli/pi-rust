//! Top-level code blocks as a plain `new Marked().lexer(...)` finds them, for source transforms
//! such as the Mermaid transformer.

use std::borrow::Cow;
use std::ops::Range;

use super::lexer::lex_top_level_blocks;
use super::token::Token;

mod cache;

pub use cache::CodeBlockCache;

/// A top-level fenced or indented code block (a marked `code` token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownCodeBlock {
    /// Source bytes of marked's `raw`: one following newline, not a longer blank run. `\r\n` is 2 bytes.
    /// A newline marked merges after a dropped duplicate definition is not contiguous, so it is left out.
    pub range: Range<usize>,
    /// The trimmed info string; empty for indented code.
    pub lang: String,
    /// The code with fence indentation removed and `\n` line endings.
    pub text: String,
}

/// Top-level code blocks of `source` under GFM `marked` 18 rules without LaTeX. Blocks in lists,
/// block quotes, or HTML blocks are skipped; an unclosed fence runs to the end of the source.
pub fn top_level_code_blocks(source: &str) -> Vec<MarkdownCodeBlock> {
    lex_spans(source, 0).0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanKind {
    Space,
    Fence,
    AtxHeading,
    Other,
}

fn span_kind(token: &Token) -> SpanKind {
    let raw = token.raw();
    let line = raw.trim_start_matches(' ');
    let indented = raw.len() - line.len() > 3;
    match token {
        Token::Space { .. } => SpanKind::Space,
        Token::Code { .. } if !indented && (line.starts_with("```") || line.starts_with("~~~")) => SpanKind::Fence,
        Token::Heading { .. } if !indented => {
            let hashes = line.bytes().take_while(|&byte| byte == b'#').count();
            let atx = (1..=6).contains(&hashes) && line[hashes..].chars().next().is_none_or(char::is_whitespace);
            if atx { SpanKind::AtxHeading } else { SpanKind::Other }
        }
        _ => SpanKind::Other,
    }
}

/// Code blocks and top-level token ranges of `source[start..]`, as offsets into `source`.
fn lex_spans(source: &str, start: usize) -> (Vec<MarkdownCodeBlock>, Vec<(SpanKind, Range<usize>)>) {
    let tail = &source[start..];
    let normalized = normalize_line_endings(tail);
    // Normalized offsets of the `\n` that replaced each `\r\n`.
    let crlf_offsets: Vec<usize> =
        tail.match_indices("\r\n").enumerate().map(|(index, (offset, _))| offset - index).collect();
    let to_source = |offset: usize| start + offset + crlf_offsets.partition_point(|&crlf| crlf < offset);
    let mut blocks = Vec::new();
    let mut spans = Vec::new();
    for (token, range) in lex_top_level_blocks(&normalized) {
        let range = to_source(range.start)..to_source(range.end);
        spans.push((span_kind(&token), range.clone()));
        if let Token::Code { lang, text, .. } = token {
            blocks.push(MarkdownCodeBlock { range, lang, text });
        }
    }
    (blocks, spans)
}

fn normalize_line_endings(source: &str) -> Cow<'_, str> {
    if source.contains('\r') {
        Cow::Owned(source.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(source)
    }
}

#[cfg(test)]
mod tests {
    use super::top_level_code_blocks;
    type CodeToken<'a> = (&'a str, &'a str, &'a str);

    fn blocks(source: &str) -> Vec<(&str, String, String)> {
        top_level_code_blocks(source).into_iter().map(|block| (&source[block.range], block.lang, block.text)).collect()
    }

    fn expected<'a>(tokens: &[CodeToken<'a>]) -> Vec<(&'a str, String, String)> {
        tokens.iter().map(|(raw, lang, text)| (*raw, lang.to_string(), text.to_string())).collect()
    }

    /// Expected `(raw, lang, text)` are the `code` tokens of `new Marked().lexer(source)`, marked 18.0.11.
    /// `tools/marked-code-blocks.mjs` prints them (see `docs/marked-code-blocks.md`).
    #[test]
    fn matches_marked_top_level_code_tokens() {
        let fence = "```mermaid\nA\n```";
        let cases: &[(&str, &[CodeToken<'_>])] = &[
            ("Before\n\n```mermaid\nA\n```\n\n\nAfter", &[(fence, "mermaid", "A")]),
            ("text\n```mermaid\nA\n```\nmore", &[("```mermaid\nA\n```\n", "mermaid", "A")]),
            ("   ```mermaid\n   A\n    B\n   ```\n", &[("   ```mermaid\n   A\n    B\n   ```\n", "mermaid", "A\n B")]),
            ("````mermaid\n```\nA\n````\n\n\nX", &[("````mermaid\n```\nA\n````", "mermaid", "```\nA")]),
            ("~~~ Mermaid extra\nA\n~~~\n", &[("~~~ Mermaid extra\nA\n~~~\n", "Mermaid extra", "A")]),
            ("```mermaid\n\n```", &[("```mermaid\n\n```", "mermaid", "")]),
            (
                "```js\nx\n```\n```Mermaid\nA\n```",
                &[("```js\nx\n```\n", "js", "x"), ("```Mermaid\nA\n```", "Mermaid", "A")],
            ),
            ("```mermaid `x`\nA\n```", &[("```", "", "")]),
            ("    ```mermaid\n    A\n    ```", &[("    ```mermaid\n    A\n    ```", "", "```mermaid\nA\n```")]),
            ("para\n    ```mermaid\n    A\n    ```", &[]),
            ("- item\n  ```mermaid\n  A\n  ```\n", &[]),
            ("> ```mermaid\n> A\n> ```", &[]),
            ("<div>\n```mermaid\nA\n```\n</div>", &[]),
            ("- item\n\n```mermaid\nA\n```", &[(fence, "mermaid", "A")]),
            ("1. a\n```mermaid\nA\n```", &[(fence, "mermaid", "A")]),
            ("> q\n```mermaid\nA\n```", &[(fence, "mermaid", "A")]),
            ("| a |\n| - |\n| b |\n```mermaid\nA\n```", &[(fence, "mermaid", "A")]),
            (
                "# H\n```mermaid\nA\n```\n- x\n```mermaid\nB\n```",
                &[("```mermaid\nA\n```\n", "mermaid", "A"), ("```mermaid\nB\n```", "mermaid", "B")],
            ),
            ("para\n[a]: x\n```mermaid\nA\n```", &[(fence, "mermaid", "A")]),
            // Without LaTeX extensions, `$$` lines are paragraphs.
            ("$$\n```mermaid\nA\n```\n$$", &[("```mermaid\nA\n```\n", "mermaid", "A")]),
            // Streaming: an unclosed fence keeps a partial closing fence in its text.
            (
                "```mermaid\nflowchart LR\n  A --> B\n``",
                &[("```mermaid\nflowchart LR\n  A --> B\n``", "mermaid", "flowchart LR\n  A --> B\n``")],
            ),
        ];
        for (source, tokens) in cases {
            assert_eq!(blocks(source), expected(tokens), "{source:?}");
        }
    }

    #[test]
    fn ranges_skip_dropped_duplicate_definitions() {
        // marked drops the second `[a]:` definition from its tokens; ranges still index the source.
        let source = "[a]: x\n\n[a]: y\n\né\n\n```mermaid\nA\n```";
        assert_eq!(blocks(source), expected(&[("```mermaid\nA\n```", "mermaid", "A")]));
        let source = "[a]: x\n[a]: y\n```mermaid\nA\n```\n";
        assert_eq!(blocks(source), expected(&[("```mermaid\nA\n```\n", "mermaid", "A")]));
    }

    /// marked drops the second definition and merges the newline after it into the code `raw`. The
    /// range stops at the contiguous bytes, so a replacement keeps `[a]: u\nnext`.
    #[test]
    fn ranges_stop_before_a_dropped_duplicate_definition() {
        let source = "[a]: u\n\n```mermaid\nflowchart LR\nA-->B\n```\n[a]: u\nnext";
        let fence = "```mermaid\nflowchart LR\nA-->B\n```\n";
        assert_eq!(blocks(source), expected(&[(fence, "mermaid", "flowchart LR\nA-->B")]));
        let raws: Vec<String> = super::lex_top_level_blocks(source)
            .filter_map(|(token, _)| matches!(token, super::Token::Code { .. }).then(|| token.raw().to_string()))
            .collect();
        assert_eq!(raws, [format!("{fence}\n")], "marked 18.0.11 code raw");
        let range = top_level_code_blocks(source)[0].range.clone();
        assert_eq!(&source[range.end..], "[a]: u\nnext");
    }

    #[test]
    fn ranges_index_the_source_with_carriage_returns() {
        let source = "Before\r\n\r\n```mermaid\r\nA\r\n```\r\nAfter";
        assert_eq!(blocks(source), expected(&[("```mermaid\r\nA\r\n```\r\n", "mermaid", "A")]));
        let source = "a\r\n\r\n```mermaid\r\nA\r\n```\r\n\r\n\r\nb\r\n";
        assert_eq!(blocks(source), expected(&[("```mermaid\r\nA\r\n```", "mermaid", "A")]));
        let source = "a\rb\r```mermaid\rA\r```\r";
        assert_eq!(blocks(source), expected(&[("```mermaid\rA\r```\r", "mermaid", "A")]));
        let source = "é\r\n\r\n```mermaid\r\nA\rB\r\n```";
        assert_eq!(blocks(source), expected(&[("```mermaid\r\nA\rB\r\n```", "mermaid", "A\nB")]));
    }
}
