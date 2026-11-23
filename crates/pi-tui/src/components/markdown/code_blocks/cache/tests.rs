//! [`CodeBlockCache`] against fresh scans of streamed, edited, and interleaved sources.

use std::io::Write;
use std::process::{Command, Stdio};

use super::super::{MarkdownCodeBlock, Token, lex_top_level_blocks, normalize_line_endings, top_level_code_blocks};
use super::CodeBlockCache;

/// Documents whose earlier tokens a later line can change, or that end inside a container.
const DOCUMENTS: &[&str] = &[
    "[a]: u\n\n```mermaid\nA\n```\n[a]: u\n[a]: u\nnext",
    "```mermaid\nflowchart LR\n  A --> B\n```\n\nAfter\n\n```mermaid\nB\n```\nTail",
    "Intro\n\n# Plan\n\n```mermaid\nA\n```\n## Next\ntext\n\n```js\nx\n```\n\nend",
    "[a]: x 'open\n```mermaid\nA\n```\nclose'\n\n```mermaid\nB\n```\n",
    "[a]: x\n\"title\n\n```mermaid\nA\n```\n\n# H\n\n\"\n```mermaid\nB\n```",
    "[a]: (paren\n\n# H\n\n```mermaid\nA\n```\n)\n\n```mermaid\nB\n```",
    "[label\n\n```mermaid\nA\n```\n]: x\n\n```mermaid\nB\n```",
    "\\[x]: y\n\n```mermaid\nA\n```\n\n[link](u) text\n\n```mermaid\nB\n```",
    "para\n-\titem\n\n```mermaid\nA\n```\n===\n\n```mermaid\nB\n```",
    "<div>\n```mermaid\nA\n```\n</div>\n\n```mermaid\nB\n```\n<!--\n```mermaid\nC\n```\n-->\nx",
    "<pre>\n\n```mermaid\nA\n```\n\n</pre>\n\n```mermaid\nB\n```",
    "- item\n\n  ```mermaid\n  A\n  ```\n\n```mermaid\nB\n```\n- next\n\n    code\n\nend",
    "> quote\n```mermaid\nA\n```\n> more\n\n```mermaid\nB\n```",
    "| a |\n| - |\n| b |\n```mermaid\nA\n```\n\n    indented\n\n```mermaid\nB\n```",
    "Before\r\n\r\n```mermaid\r\nA\r\n```\r\n\r\n# H\r\nafter\r```mermaid\rB\r```\r\ntail",
    "#hashtag\n===\n\n```mermaid\nA\n```\n#\n````\n```mermaid\n````\n~~~mermaid\nB",
    "é\n\n```mermaid `x`\nA\n```\n\n    ```mermaid\n    A\n    ```\n\n```mermaid\n\n```",
];

fn char_prefixes(document: &str) -> impl Iterator<Item = &str> {
    document.char_indices().map(|(index, _)| &document[..index]).chain([document])
}

fn line_prefixes(document: &str) -> impl Iterator<Item = &str> {
    document.match_indices(['\n', '\r']).map(|(index, _)| &document[..=index]).chain([document])
}

fn check(cache: &mut CodeBlockCache, source: &str) -> Vec<MarkdownCodeBlock> {
    let cached = cache.top_level_code_blocks(source);
    assert_eq!(cached, top_level_code_blocks(source), "{source:?}");
    cached
}

#[test]
fn streamed_prefixes_match_fresh_scans() {
    for document in DOCUMENTS {
        let mut cache = CodeBlockCache::default();
        char_prefixes(document).for_each(|prefix| _ = check(&mut cache, prefix));
        let mut cache = CodeBlockCache::default();
        line_prefixes(document).for_each(|prefix| _ = check(&mut cache, prefix));
        // The interactive mode trims each streamed text.
        let mut cache = CodeBlockCache::default();
        char_prefixes(document).for_each(|prefix| _ = check(&mut cache, prefix.trim()));
    }
}

#[test]
fn interleaved_edited_and_shrinking_sources_match_fresh_scans() {
    let mut cache = CodeBlockCache::default();
    for pair in DOCUMENTS.windows(2) {
        let (first, second): (Vec<&str>, Vec<&str>) =
            (char_prefixes(pair[0]).collect(), char_prefixes(pair[1]).collect());
        for index in 0..first.len().max(second.len()) {
            check(&mut cache, first[index.min(first.len() - 1)]);
            check(&mut cache, second[index.min(second.len() - 1)]);
        }
        let mut edited = pair[0].to_string();
        edited.insert_str(edited.len() / 2, "[a]: '\n");
        check(&mut cache, &edited);
        edited.replace_range(..1, "\n");
        check(&mut cache, &edited);
        check(&mut cache, &pair[0][..pair[0].len() / 3]);
        check(&mut cache, pair[0]);
    }
}

#[test]
fn split_carriage_return_line_feed_matches_fresh_scans() {
    let mut cache = CodeBlockCache::default();
    for source in
        ["# H\r", "# H\r\n```mermaid\r", "# H\r\n```mermaid\r\nA\r\n```\r", "# H\r\n```mermaid\r\nA\r\n```\r\nx"]
    {
        check(&mut cache, source);
    }
}

/// xorshift64*, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % bound
    }
}

const FRAGMENTS: &[&str] = &[
    "```mermaid",
    "```",
    "~~~",
    "````",
    "```js",
    "  ```mermaid",
    "    ```mermaid",
    "A --> B",
    "# Heading",
    "#",
    "#tag",
    "======",
    "---",
    "***",
    "- item",
    "-\titem",
    "1. one",
    "2. two",
    "  continued",
    "> quote",
    ">",
    "[a]: x",
    "[a]: x \"title",
    "title\"",
    "[a]: x 'open",
    "close'",
    "[a]: (paren",
    "paren)",
    "[b",
    "]: y",
    "[link](u) text",
    "<div>",
    "</div>",
    "<!--",
    "-->",
    "<pre>",
    "</pre>",
    "| a | b |",
    "| - | - |",
    "    indented",
    "\tTabbed",
    "",
    "   ",
    "para text",
    "$$",
    "é",
];

fn random_document(rng: &mut Rng) -> String {
    let mut document = String::new();
    for _ in 0..2 + rng.below(24) {
        document.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
        document.push_str(["\n", "\n", "\n", "\n", "\r\n", "\r"][rng.below(6)]);
    }
    document.truncate(document.len() - rng.below(2));
    document
}

fn random_documents(count: usize) -> Vec<String> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..count).map(|_| random_document(&mut rng)).collect()
}

#[test]
fn random_documents_streamed_in_random_chunks_match_fresh_scans() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let mut cache = CodeBlockCache::default();
    for document in random_documents(600) {
        let mut end = 0;
        while end < document.len() {
            end = (end + 1 + rng.below(12)).min(document.len());
            while !document.is_char_boundary(end) {
                end += 1;
            }
            check(&mut cache, &document[..end]);
        }
    }
}

#[test]
fn streaming_lexes_the_tail_after_the_last_resume_point() {
    let paragraph = "Prose with `code`, *emphasis*, and a [link](https://example.invalid) in it.\n\n";
    let diagram = "```mermaid\nflowchart LR\n  A --> B\n  B --> C\n```\n\n";
    let shapes = [
        format!("{}{diagram}", paragraph.repeat(60)),
        format!("{diagram}{}", paragraph.repeat(60)),
        format!("# Title\n\n{diagram}{}## Part\n\n{diagram}", paragraph.repeat(30)),
    ];
    for document in shapes {
        let mut cache = CodeBlockCache::default();
        let (mut end, mut rescan_bytes) = (0, 0);
        while end < document.len() {
            end = (end + 16).min(document.len());
            let source = document[..end].trim();
            check(&mut cache, source);
            rescan_bytes += source.len();
        }
        assert!(cache.lexed_bytes * 8 < rescan_bytes, "lexed {} of {rescan_bytes}", cache.lexed_bytes);
    }
}

#[test]
fn a_definition_label_stops_reuse_before_it() {
    let mut cache = CodeBlockCache::default();
    check(&mut cache, "# A\n\n[a]: x\n\n```mermaid\nA\n```\n\nmore");
    let lexed = cache.lexed_bytes;
    let source = "# A\n\n[a]: x\n\n```mermaid\nA\n```\n\nmore text";
    check(&mut cache, source);
    assert_eq!(cache.lexed_bytes - lexed, source.len() - "# A\n\n".len());
}

#[test]
fn scans_past_the_byte_limit_are_not_kept() {
    let mut cache = CodeBlockCache { byte_limit: 4096, ..CodeBlockCache::default() };
    check(&mut cache, "```mermaid\nA\n```\n\nsmall");
    check(&mut cache, &format!("```mermaid\nA\n```\n\n{}", "word\n".repeat(1000)));
    assert_eq!(cache.scans.len(), 1);
    check(&mut cache, &format!("```mermaid\nA\n```\n\n{}", "word\n".repeat(600)));
    check(&mut cache, &format!("```mermaid\nA\n```\n\n{}", "more\n".repeat(600)));
    assert_eq!(cache.scans.len(), 1);
    assert!(cache.scans.iter().map(super::Scan::retained_bytes).sum::<usize>() <= 4096);
}

/// Runs `tools/marked-code-blocks.mjs` on the vendored marked 18.0.11. Run with
/// `cargo test -p pi-tui --lib -- --ignored code_blocks::cache::tests::streamed_prefixes_match_marked`.
#[test]
#[ignore = "needs Node"]
fn streamed_prefixes_match_marked() {
    let mut sources: Vec<String> = DOCUMENTS
        .iter()
        .flat_map(|document| char_prefixes(document).chain(line_prefixes(document)))
        .map(String::from)
        .collect();
    for document in random_documents(300) {
        sources.extend(char_prefixes(&document).map(String::from));
    }
    let tool = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/marked-code-blocks.mjs");
    let mut child =
        Command::new("node").arg(tool).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("node runs");
    let input = serde_json::to_vec(&sources).expect("sources serialize");
    let mut stdin = child.stdin.take().expect("stdin");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().expect("marked output");
    writer.join().expect("writer").expect("input written");
    assert!(output.status.success(), "marked-code-blocks.mjs failed");
    let expected: Vec<Vec<(String, String, String)>> = serde_json::from_slice(&output.stdout).expect("marked JSON");
    assert_eq!(expected.len(), sources.len());
    let mut cache = CodeBlockCache::default();
    for (source, marked) in sources.iter().zip(&expected) {
        let cached = cache.top_level_code_blocks(source);
        assert_eq!(cached, top_level_code_blocks(source), "cached source spans for {source:?}");
        let tokens: Vec<_> = lex_top_level_blocks(&normalize_line_endings(source))
            .filter_map(|(token, _)| match token {
                Token::Code { raw, lang, text, .. } => Some((raw, lang, text)),
                _ => None,
            })
            .collect();
        assert_eq!(&tokens, marked, "marked tokens for {source:?}");
        assert_eq!(cached.len(), marked.len(), "{source:?}");
        for (block, (raw, lang, text)) in cached.iter().zip(marked) {
            let rest = normalize_line_endings(&source[block.range.start..]);
            let contiguous = raw.bytes().zip(rest.bytes()).take_while(|(a, b)| a == b).count();
            let slice = normalize_line_endings(&source[block.range.clone()]);
            assert_eq!(slice.as_bytes(), &raw.as_bytes()[..contiguous], "replacement range for {source:?}");
            assert!(raw.as_bytes()[contiguous..].iter().all(|&byte| byte == b'\n'), "marked raw for {source:?}");
            assert_eq!((&block.lang, &block.text), (lang, text), "{source:?}");
        }
    }
}
