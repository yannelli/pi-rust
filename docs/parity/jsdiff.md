# Edit diffs and jsdiff

Created: 2026-11-21. Last updated: 2026-11-21.

Upstream pi makes its edit diffs with the npm package `diff` ([jsdiff](https://github.com/kpdecker/jsdiff)). The synced upstream commit uses version 8.0.4 (`packages/coding-agent/package.json`). pi-rust has a port of the jsdiff code that pi uses, so the diffs are the same as upstream.

| pi-rust | Upstream and jsdiff |
| --- | --- |
| `pi_durable::diff::diff_tokens` ([mod.rs](../../crates/pi-durable/src/diff/mod.rs)) | `Diff.diff`: the greedy Myers search, its tie-breaks, and its diagonal pruning |
| `generate_diff_string`, `generate_unified_patch`, `generate_edit_diff` ([lines.rs](../../crates/pi-durable/src/diff/lines.rs)) | Upstream `generateDiffString` and `generateUnifiedPatch`: `diffLines`, and `createTwoFilesPatch` with `FILE_HEADERS_ONLY` and the context size of the caller (default 4) |
| `diff_words` ([words.rs](../../crates/pi-durable/src/diff/words.rs)), used by the diff view | `diffWords` and its whitespace cleanup, `dedupeWhitespaceInChangeObjects` |

Extensions can give `contextLines` to `generateDiffString` and `generateUnifiedPatch`. The SDK accepts a non-negative integer or `Infinity`, and throws `ERR_PI_UNSUPPORTED` for other values.

## Rules

- Word diffs use JavaScript `\s` for whitespace (`is_js_whitespace`). Rust `char::is_whitespace` gives different results for U+0085 and U+FEFF.
- Do not remove the common suffix before the search. jsdiff does not, and the result changes which of the repeated lines the diff marks.
- jsdiff keeps each search path as a linked list of change objects and lets the garbage collector free them. The port keeps the lists in an arena and compacts the arena when it gets large.

## When upstream changes the `diff` version

1. Read the jsdiff release notes for changes to `diffLines`, `diffWords`, `structuredPatch`, and `formatPatch`. Also look for new jsdiff functions or options that upstream starts to use.
2. Compare the port with the new version. The comparison does not run in CI.

   ```sh
   mkdir -p /tmp/jsdiff && cd /tmp/jsdiff && npm pack diff@<version> && tar -xzf diff-<version>.tgz
   cd <pi-rust>
   node scripts/jsdiff-oracle.mjs <upstream pi checkout> /tmp/jsdiff/package > /tmp/jsdiff/oracle.json
   PI_JSDIFF_ORACLE=/tmp/jsdiff/oracle.json cargo test -p pi-durable --lib --release matches_jsdiff_oracle -- --ignored
   ```

   The script runs upstream `generateDiffString`, `createTwoFilesPatch`, and `diffWords` on about 38,000 generated inputs. The test compares each result with the port. Set `SEED` to try other inputs.
3. Copy new results into the expected values in [tests.rs](../../crates/pi-durable/src/diff/tests.rs) and in the tests of `words.rs`. If the large case changes, put the new `largeSha256` of the script output into `large_diff_matches_upstream`.

The check on 2026-11-21 used jsdiff 8.0.4 and found no differences in 7,935 line cases and 30,225 word cases.

## License

jsdiff uses the BSD 3-Clause license, and the port is a close translation of its code, so pi-rust keeps the notice. The notice is in [LICENSE-jsdiff](../../crates/pi-durable/src/diff/LICENSE-jsdiff) and in the README license table. `about.hbs` adds it to `THIRD-PARTY-LICENSES.md`, which goes into each release archive.

To do: `release.yml` copies the `LICENSE-*` file of each other ported part into the release archives. Add `crates/pi-durable/src/diff/LICENSE-jsdiff` to that `cp` line with a token that has the `workflow` scope.
