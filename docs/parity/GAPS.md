# Open parity gaps

Last reviewed 2026-10-21 against upstream `main` at `82432219a` (2026-10-21). The daily [parity report](README.md#daily-gap-check) finds new gaps; this file keeps the ones that need a decision or a plan. Remove an entry in the PR that closes it.

## Upstream commits not yet ported

None up to `82432219a`. Every commit after the synced commit is ported or listed in `allowlist.json` with a reason.

## Deferred work

- **chord2 runtime** (upstream `0104fef1e` and later chord commits). pi-chord ports the earlier facets runtime. This is the largest open item; plan it as its own branch.
- **SQLite telemetry store** (the chord `telemetry-sqlite` half of upstream `d9025a5f5`). pi-telemetry has the in-memory `TelemetryStore` and the trace renderer. Upstream keeps the SQLite facade in chord; here pi-durable owns SQLite, so the store needs a home first.
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

## Platform

- `pi experimental server` and `client` use Unix sockets, so they report that they need Unix on Windows.

## Test reliability

- `crates/pi-js/tests/node_tls.rs` (`tls_matches_node`) can fail with an uncaught `ECONNRESET` under parallel load. A local stress run on 2026-10-14 failed 7 of 60 runs. The likely cause is the client closing before it reads the server's session tickets. Fix it, then stress it with the loop in [CI triage](../agents/ci-triage.md#hangs-and-flaky-tests) before you call it fixed.
