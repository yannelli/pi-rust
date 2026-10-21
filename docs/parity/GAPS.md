# Open parity gaps

Last reviewed 2026-10-21 against upstream `main` at `bd3c9b823` (2026-10-21). The daily [parity report](README.md#daily-gap-check) finds new gaps; this file keeps the ones that need a decision or a plan. Remove an entry in the PR that closes it.

## Upstream commits not yet ported

None up to `bd3c9b823`. Every commit after the synced commit is ported or listed in `allowlist.json` with a reason.

## Deferred work

- **chord2 tutorial test** (upstream `test/tutorial.test.ts`). pi-chord's `host2` module ports the chord2 runtime (upstream `0104fef1e` and later chord commits): errors, schemas, contracts, wire frames and links, the host manager and process locks, cells, objects and hosts, connections with remote references and flow control, the WebSocket link, and reloads, code version retirement and clusters. The tutorial walkthrough is not ported yet.
- **`HarnessOptions.models` as a function** (upstream `7ec1f648e`). Not planned: settings are already a function here, and a `Models` implementation can change its catalog at runtime. It is older than the synced commit, so the report does not list it.

## chord2 differences

How pi-chord's `host2` differs from upstream chord on purpose:

- Hosts get facets from a `FacetLoader`, because upstream's Node module loader (`loader.ts`) cannot be ported. So `7d75a0e3e` (the loader registers every facet root before it imports one) does not apply. A cluster reload's children load the named modules through their own `FacetLoader`.
- Factories are synchronous, and `Subscribable::subscribe` cannot fail: a listener's `select` or its resource fails instead.
- `Declare::on_select` takes a key for the identity of `select`, where upstream compares the function's source text. A reload keeps the subscription while the resource and the key stay the same.
- `Host::host_object` returns the host's own reference to a `lifetime: "host"` object (upstream tests read the private `hostObjects`).
- Upstream `4a5d37291` moved `OutputBuffer` and `Progress` into `@earendil-works/chord/output`. pi-rust keeps them in pi-durable (`harness/output.rs` and `harness/progress.rs`).
- Tests: fake timers are real short timers, and `Weak` sentinels replace `FinalizationRegistry` with a forced GC. The review test "a commit that still fails midway" is not ported: it patches an object's `startTasks`, which Rust code cannot do.

## Upstream commits that do not apply

Older than the synced commit, so the report no longer lists them.

- `ea25697e9` managed install markers with a BOM: `pi update --self` replaces the release binary in place (update/install.rs); there is no managed-install.json marker.
- `16f5f0af4` IndexedDbStorage: a browser storage backend, and pi-rust has no browser build.
- `c661bc574` drops the shared MCP types block from durable code mode: pi-codemode never had it, so the port of `8bb180bec` already renders the section without it.
- `4c25119f5` replicates durable storages as a commit log, and `cae9fd28a` reverts it: the pair is a net zero change.

## Extension runtime (pi-js)

`crates/pi-js/src/unsupported.rs` is the full list. Open items that need a decision:

- **TLS `pfx`** needs a PKCS#12 parser, which is a new dependency. Ask before adding one.
- **TLS before 1.2** cannot work: rustls supports TLS 1.2 and 1.3 only.
- **Byte-identical zlib output**: zlib-rs output can differ in bytes from Node's zlib, and `memLevel` and `strategy` are not applied.
- **V8-only features**: heap snapshots with nodes, `v8.queryObjects`, `inspector.open`, and full `async_hooks` coverage of timers, promises, and I/O depend on V8 internals that QuickJS does not have.

## Removed upstream, still in pi-rust

Upstream `7472cda0d` (2026-10-12) deleted the experimental server stack: the client, protocol, and server packages and `pi experimental`. pi-rust keeps them on purpose as pi-rust extras (decided 2026-10-21): `pi-client`, `pi-protocol`, `pi-server`, the `pi experimental` commands (`server`, `client`, `durable`, `vacation`), and the facets/services runtime in pi-chord that they use. The three crates are in `allowlist.json`. They get no upstream fixes from now on.

## pi-rust extensions

Behavior that pi-rust adds on purpose. The parity review should not report these as gaps.

- Account model list for `openai-codex` from the Codex backend's `/codex/models` (README "Account model lists").
- Account model list for `anthropic` from `/v1/models` (README "Account model lists").
- Account model list for `github-copilot` from the Copilot `/models` endpoint (README "Account model lists").
- Account model lists for `openai` and `xai` from `/models`, and for `google` from the Gemini API `/models` (README "Account model lists"). Cursor and Grok Build have no public model list, so they are not narrowed.
- Markdown export: `/export` and `pi --export` write Markdown when the output path ends in `.md` (README "Use").

## Platform

- `pi experimental server` and `client` use Unix sockets, so they report that they need Unix on Windows.
- The chord2 host manager gives a child its IPC channel as stdin on Unix (upstream uses an extra file descriptor) and as a named pipe on Windows. On Windows, `kill_tree` ends only the child, as upstream does there.
