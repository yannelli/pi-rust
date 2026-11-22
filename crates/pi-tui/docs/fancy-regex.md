# fancy-regex in the markdown lexer

Created: 2026-11-21. Last updated: 2026-11-21.

The markdown lexer in `crates/pi-tui/src/components/markdown/` uses `fancy-regex` 0.19 for the marked rules that need lookaround or backreferences. This note records how that crate fails and how the lexer handles it. Source: the `fancy-regex` 0.19.2 code (`src/lib.rs`, `src/vm.rs`) and the measurements below. Issue: [#252](https://github.com/yannelli/pi-rust/issues/252).

## Rules for markdown code

- Do not call `replace`, `replace_all`, or `replacen` on a fancy rule. They call `try_replacen(...).unwrap()` and panic on an engine error. Use `rules::replace_all`. It returns the text unchanged on an error.
- Use the helpers in `rules.rs` for single matches: `captures`, `is_match`, and `captures_anywhere`. They treat an engine error as no match.
- A tokenizer that returns `None` makes the inline loop stop, and the loop drops the rest of the source. A rule that matches all input, such as `INLINE_TEXT`, needs a fallback for an engine error. `inline_text` lexes a word-aligned prefix of at most 64 KB (`TEXT_CHUNK_BYTES`).
- The rules use the default backtrack limit of 1M. Do not use the `seek` builder option. The crate marks it experimental, and its docs say it can skip matches.

## How the backtrack limit works

- The limit applies to one search call. The VM counts backtracks for the full unanchored scan and does not reset the count at each start position.
- On plain prose the marked rules use fewer than 4 backtracks per byte. Thus `BLOCK_SKIP`, `REFLINK_SEARCH`, and `INLINE_TEXT` fail at about 300 KB of text with no code span, link, or tag. Each match starts a new search, so text with many code spans gets farther.
- `find_iter` and `captures_iter` stop after the first `Err`. Loops that use `.flatten()` or `break` on an error do not run forever.

## What a long paragraph does

| Input of about 420 KB | Rules that fail | Result |
| --- | --- | --- |
| One line of prose | `BLOCK_SKIP`, `INLINE_TEXT` | All text renders |
| 11,700 lines of prose, no blank line | `BLOCK_SKIP`, `INLINE_TEXT` | All text renders |

`PARAGRAPH` and `LHEADING` did not fail on these inputs. When `BLOCK_SKIP` fails, the lexer does not mask code spans, links, and tags in that paragraph. Then an emphasis delimiter inside a code span can pair with one outside it.

## How the chunk split was checked

The `INLINE_TEXT` lookaheads (`http`, `www.`, email, and two spaces before a newline) cannot cross whitespace. A prefix that ends at the start of a word thus gives the same tokens. A test run forced every text run through 16-byte chunks, and all 63 markdown and LaTeX tests passed.
