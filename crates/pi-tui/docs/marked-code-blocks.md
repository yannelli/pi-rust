# Top-level code blocks and the marked oracle

Created: 2026-11-22. Last updated: 2026-11-22.

`top_level_code_blocks` in `src/components/markdown/code_blocks.rs` returns the top-level `code` tokens that `new Marked().lexer(source)` returns in marked 18.0.11. The Mermaid transformer in pi-coding-agent uses it to find diagrams. `CodeBlockCache` in `code_blocks/cache.rs` keeps recent scans, so a streamed message is lexed again only from its last resume point.

## Run the marked oracle

Run the checks from the repository root. Node reads the pinned marked 18.0.11 bundle from `crates/pi-coding-agent/src/export/assets/vendor/marked.min.js`.

- `node crates/pi-tui/tools/marked-code-blocks.mjs < sources.json` takes a JSON array of Markdown strings and prints one array of `[raw, lang, text]` tuples for each string. The tool checks the bundled file's SHA-256. An optional path selects another marked build for comparison.
- `cargo test -p pi-tui --lib -- --ignored code_blocks::cache::tests::streamed_prefixes_match_marked` compares the Rust tokens with marked, checks the source ranges, and compares cached with fresh parses on streamed inputs.
- To update the fixtures in `matches_marked_top_level_code_tokens`, pass their source strings to the Node tool.

After a fenced block, a duplicate link definition can make marked append a newline to the token's `raw` text. That newline belongs after the definition in the original document. The replacement range stops at the last contiguous source byte so the definition survives diagram rendering. The oracle checks the complete marked token and the original byte range separately.

## Why resume points are stable

A resume point is a top-level token start that text added later cannot change. The tokens before the resume point stay the same, and lexing can start again there with a new lexer. The resume points are:

- The start of a fenced code token or an ATX heading token.
- The start of a non-blank token after a blank run, a fenced code token, or an ATX heading.

The first line of the token at the resume point must be complete (it ends with `\n` or `\r`), and the new source must contain the same bytes up to the end of that line.

These block rules stop at a complete fence line, at an ATX heading line, and at a blank line: paragraph, setext heading, table rows, block quote continuation, list items, indented code, and HTML blocks of types 6 and 7. A fence or an HTML block of types 1 to 5 that is not closed runs to the end of the source, so no token start comes after it.

No token at a resume point merges into the token before it. Only a one-byte blank line, indented code, and a definition merge backward. The resumed lexer has an empty link map, so it keeps a duplicate definition that the full lexer drops. This does not matter, because a definition before the resume point stops reuse (see the next paragraph).

A link definition can cross lines, blank lines, and fences through its label and its `"..."` or `(...)` title. The cache records each line outside code blocks that starts with up to three spaces and `[`. If the label on such a line ends with `]:` in the new source, lexing starts again at or before that line.

Lists and block quotes are not resume points. A setext heading can continue through a `-\t` list line, because its interrupt list differs from the paragraph interrupt list.

## Limits

- A cache keeps at most 8 scans and 16 MiB. The byte count includes the capacity of the source, the block text, and the offset vectors. A larger scan is returned but not kept.
- A long run of text with no blank line, fence, or heading has no resume point inside it. The whole run is lexed again on each update. With an 85 KB single paragraph, the transform takes 6.7 ms for each update.
