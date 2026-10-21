# Open parity gaps

Last reviewed 2026-10-21 against upstream `main` at `16f5f0af4` (2026-10-21). The daily [parity report](README.md#daily-gap-check) finds new gaps; this file keeps the ones that need a decision or a plan. Remove an entry in the PR that closes it.

## Upstream commits not yet ported

These upstream commits come after the newest ported commit (`d2e7d8b7c`, 2026-10-19). Some can already be in pi-rust without naming the upstream SHA; confirm before you port.

| Area | Upstream commits |
| --- | --- |
| pi-ai | `73f8de80b` Copilot request-size overflow, `caa6967b6` prompt cache across tool changes on Responses, `530015d93` only announced removals stay unavailable, `fdce48eb4` drop Codex models rejected for ChatGPT accounts |
| pi-coding-agent | `28bea006b` Windows Store shell aliases, `61060ddf8` Windows shells from installation directories, `ea25697e9` managed install markers with a BOM, `26293e74f` CJK emphasis in the HTML export, `38847563f` stack concurrent extension dialogs, `f8ab362cc` clipboard helpers outside pi's process group, `885a564b3` queue codemode scripts at the worker limit, `22dc6c26f` expose `@earendil-works/pi-ai/models` to extensions, `bdd9f0399` `mcp.json` with a BOM |
| pi-tui | `0ed1b0431` Rex terminal OSC 8 links, `396ecd7b8` Kitty images for Rex, `b2d0da6fe` CJK emphasis next to punctuation, `2ed3118b0` unclosed display math and tables, `9df819de3` restore terminal modes on exit without `stop()` |
| pi-chord | `d9025a5f5` SQLite telemetry store and trace renderer, `48c8f3228` exclusive SQLite operations and a replicated connection |
| pi-durable | `5c1736f82` `checkpointWhen` required on document definitions, `16f5f0af4` IndexedDbStorage (browser storage; check whether it applies) |

## Deferred work

- **chord2 runtime** (upstream `0104fef1e` and later chord commits). pi-chord ports the earlier facets runtime. Its `host2` module has the chord2 foundation (errors, schemas, contracts, wire frames, and memory links) and the host manager with IPC links, process locks, and the `FacetLoader` trait. Still open: cells, objects, and hosts; connections, remotes, and the WebSocket link; reload and clusters. Upstream's Node module loader (`loader.ts`) cannot be ported, so hosts get facets from a `FacetLoader`.
- **`HarnessOptions.models` as a function** (upstream `7ec1f648e`). Not planned: settings are already a function here, and a `Models` implementation can change its catalog at runtime. It is older than the synced commit, so the report does not list it.

## Extension runtime (pi-js)

`crates/pi-js/src/unsupported.rs` is the full list. Open items that need a decision:

- **TLS `pfx`** needs a PKCS#12 parser, which is a new dependency. Ask before adding one.
- **TLS before 1.2** cannot work: rustls supports TLS 1.2 and 1.3 only.
- **Byte-identical zlib output**: zlib-rs output can differ in bytes from Node's zlib, and `memLevel` and `strategy` are not applied.
- **V8-only features**: heap snapshots with nodes, `v8.queryObjects`, `inspector.open`, and full `async_hooks` coverage of timers, promises, and I/O depend on V8 internals that QuickJS does not have.

## Removed upstream, still in pi-rust

Upstream `7472cda0d` (2026-10-12) deleted the experimental server stack: the client, protocol, and server packages, `pi experimental server` and `client`, and the example plugin. pi-rust still ships `pi-client`, `pi-protocol`, `pi-server`, and the `pi experimental` commands, so the parity report lists those crates as only in pi-rust. Decide whether to remove them to match upstream or keep them and add them to `allowlist.json`.

This commit is older than the synced commit, so the commits section treats it as ported. Commits between the fork snapshot (2026-10-09) and `d2e7d8b7c` were ported one at a time. Check that window for other skipped commits.

## pi-rust extensions

Behavior that pi-rust adds on purpose. The parity review should not report these as gaps.

- Account model list for `openai-codex` from the Codex backend's `/codex/models` (README "Account model lists").
- Account model list for `anthropic` from `/v1/models` (README "Account model lists").
- Account model list for `github-copilot` from the Copilot `/models` endpoint (README "Account model lists").
- Account model lists for `openai` and `xai` from `/models`, and for `google` from the Gemini API `/models` (README "Account model lists"). Cursor and Grok Build have no public model list, so they are not narrowed.

## Platform

- `pi experimental server` and `client` use Unix sockets, so they report that they need Unix on Windows.
- The chord2 host manager gives a child its IPC channel as stdin on Unix (upstream uses an extra file descriptor) and as a named pipe on Windows. On Windows, `kill_tree` ends only the child, as upstream does there.

## Test reliability

- `crates/pi-js/tests/node_tls.rs` (`tls_matches_node`) can fail with an uncaught `ECONNRESET` under parallel load. A local stress run on 2026-10-14 failed 7 of 60 runs. The likely cause is the client closing before it reads the server's session tickets. Fix it, then stress it with the loop in [CI triage](../agents/ci-triage.md#hangs-and-flaky-tests) before you call it fixed.
