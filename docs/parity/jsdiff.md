# Edit diffs and jsdiff

Created: 2026-11-21. Last updated: 2026-11-21.

Upstream pi makes its edit diffs with the npm package `diff` ([jsdiff](https://github.com/kpdecker/jsdiff)). The synced upstream commit uses version 8.0.4 (`packages/coding-agent/package.json`). pi-rust has a port of the jsdiff code that pi uses, so the diffs are the same as upstream.

| pi-rust | Upstream and jsdiff |
| --- | --- |
| `pi_durable::diff::diff_tokens` ([mod.rs](../../crates/pi-durable/src/diff/mod.rs)) | `Diff.diff`: the greedy Myers search, its tie-breaks, and its diagonal pruning |
| `generate_diff_string`, `generate_unified_patch`, `generate_edit_diff` ([lines.rs](../../crates/pi-durable/src/diff/lines.rs)) | Upstream `generateDiffString` and `generateUnifiedPatch`: `diffLines`, and `createTwoFilesPatch` with context 4 and `FILE_HEADERS_ONLY` |
| `diff_words` ([word_diff.rs](../../crates/pi-coding-agent/src/interactive/components/word_diff.rs)) | `diffWords` and its whitespace cleanup, `dedupeWhitespaceInChangeObjects` |

## Rules

- Word diffs use JavaScript `\s` for whitespace (`is_js_whitespace`). Rust `char::is_whitespace` gives different results for U+0085 and U+FEFF.
- Do not remove the common suffix before the search. jsdiff does not, and the result changes which of the repeated lines the diff marks.
- jsdiff keeps each search path as a linked list of change objects and lets the garbage collector free them. The port keeps the lists in an arena and compacts the arena when it gets large.

## When upstream changes the `diff` version

1. Read the jsdiff release notes for changes to `diffLines`, `diffWords`, `structuredPatch`, and `formatPatch`.
2. Compare the port with the new version. Run upstream `generateDiffString`, `createTwoFilesPatch`, and `renderIntraLineDiff` in Node on random inputs, run the Rust functions on the same inputs, and compare the strings. The check on 2026-11-21 used jsdiff 8.0.4 and found no differences in 7,354 line pairs and 46,349 word pairs.
3. Copy the new results into the expected values in [tests.rs](../../crates/pi-durable/src/diff/tests.rs) and in the tests of `word_diff.rs`.

## License

jsdiff uses the BSD 3-Clause license. The notice is in [LICENSE-jsdiff](../../crates/pi-durable/src/diff/LICENSE-jsdiff). `about.hbs` adds it to `THIRD-PARTY-LICENSES.md`, which goes into each release archive.
