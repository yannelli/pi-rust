# Open parity gaps

Last reviewed 2026-10-27 against upstream `main` at `531340f10` (2026-10-27). The daily [parity report](README.md#daily-gap-check) finds new gaps; this file keeps the ones that need a decision or a plan. Remove an entry in the PR that closes it.

## Upstream commits not yet ported

None up to `531340f10`. Every commit after the synced commit is ported or listed in `allowlist.json` with a reason.

## Deferred work

- **One copy of the partial message per streamed delta.** Since upstream `f715a15c1`, tool-call arguments are parsed once per delta (`pi_ai::streaming_json`). Each `AssistantMessageEvent` still carries a full copy of the partial message, so a very large tool call costs more per delta as it grows. Removing that copy would change the public event API.
- **`HarnessOptions.models` as a function** (upstream `7ec1f648e`). Not planned: settings are already a function here, and a `Models` implementation can change its catalog at runtime. It is older than the synced commit, so the report does not list it.

## chord2 differences

How pi-chord's `host2` differs from upstream chord on purpose:

- Hosts get facets from a `FacetLoader`, because upstream's Node module loader (`loader.ts`) cannot be ported. So `7d75a0e3e` (the loader registers every facet root before it imports one) does not apply. A cluster reload's children load the named modules through their own `FacetLoader`.
- Factories are synchronous, and `Subscribable::subscribe` cannot fail: a listener's `select` or its resource fails instead.
- `Declare::on_select` takes a key for the identity of `select`, where upstream compares the function's source text. A reload keeps the subscription while the resource and the key stay the same.
- `Host::host_object` returns the host's own reference to a `lifetime: "host"` object (upstream tests read the private `hostObjects`).
- Upstream `4a5d37291` moved `OutputBuffer` and `Progress` into `@earendil-works/chord/output`. pi-rust keeps them in pi-durable (`harness/output.rs` and `harness/progress.rs`).
- `Change::append_text` (upstream `appendText`) records an append op without comparing the strings, but the new string is still a copy: a JSON string is an immutable shared `Arc<str>`, so each append costs O(length), not O(text). It takes the path of the object that holds the key, since a Rust draft is a `&mut Json` and not a proxy.
- Durable calls `subscribe_commits()` listeners inside Storage's `adopt`, right after the Session's own observers, where upstream (`8b0caefc2`) runs them one microtask later: Rust has no microtask queue, and a spawned task could run after the waits that the commit resolves. A telemetry `add_event` or metric cannot fail in Rust, so there is no `host_error` report for `"telemetry"`.
- Tests: fake timers are real short timers, and `Weak` sentinels replace `FinalizationRegistry` with a forced GC. The review test "a commit that still fails midway" is not ported: it patches an object's `startTasks`, which Rust code cannot do.
- The tutorial test (upstream `test/tutorial.test.ts`) is `chord2_tutorial.rs` and `chord2_tutorial_reload.rs`, one test per section. Its drift check, which compares `docs/tutorial.md` with the TypeScript examples in the test, is not ported: pi-rust has no TypeScript copy of the examples.

## Upstream commits that do not apply

Older than the synced commit, so the report no longer lists them.

- `ea25697e9` managed install markers with a BOM: `pi update --self` replaces the release binary in place (update/install.rs); there is no managed-install.json marker.
- `16f5f0af4` IndexedDbStorage: a browser storage backend, and pi-rust has no browser build.
- `c661bc574` drops the shared MCP types block from durable code mode: pi-codemode never had it, so the port of `8bb180bec` already renders the section without it.
- `4c25119f5` replicates durable storages as a commit log, and `cae9fd28a` reverts it: the pair is a net zero change.
- `2685de7a1`, `66c2fa3d7`, and `936e5f40a` update or drop npm dependencies. pi-rust has no npm dependencies.
- `476a303c1` drops an `npm ci` override for managed installs. `pi update --self` downloads release archives, not npm.
- `c8ec752cf` skips `fd` and `rg` downloads on unsupported architectures. pi-rust never downloads them, because `grep` and `find` use the `ignore` crate.
- `531340f10` deletes root npm shim files of the TypeScript ai package. pi-rust has no npm package layout.
- The V8 string retention parts of `f715a15c1` (chord `ownedSlice`, bounded error excerpts) do not apply, because Rust strings own their bytes. The rest of the commit is ported.

## Extension runtime (pi-js)

`crates/pi-js/src/unsupported.rs` is the full list. Open items that need a decision:

- **TLS `pfx`** needs a PKCS#12 parser, which is a new dependency. Ask before adding one.
- **TLS before 1.2** cannot work: rustls supports TLS 1.2 and 1.3 only.
- **Byte-identical zlib output**: zlib-rs output can differ in bytes from Node's zlib, and `memLevel` and `strategy` are not applied.
- **V8-only features**: heap snapshots with nodes, `v8.queryObjects`, `inspector.open`, and full `async_hooks` coverage of timers, promises, and I/O depend on V8 internals that QuickJS does not have.

## Removed upstream, still in pi-rust

Upstream `7472cda0d` (2026-10-12) deleted the experimental server stack: the client, protocol, and server packages and `pi experimental`. pi-rust keeps them on purpose as pi-rust extras (decided 2026-10-21): `pi-client`, `pi-protocol`, `pi-server`, the `pi experimental` commands (`server`, `client`, `durable`, `vacation`), and the facets/services runtime in pi-chord that they use. The three crates are in `allowlist.json`. They get no upstream fixes from now on.

Upstream `92ab53602` removed the Chord state getters `Session.documentState()`, `Conversation.viewState()`, and `Harness.taskGraph()` with the `DocumentState` type. pi-durable keeps them because the experimental stack in pi-coding-agent reads conversations and the task graph through them. When a document is retired, its document state publishes `null`. When a new document is created at the same address, the state publishes that document's value, the same way a watch follows its address.

## pi-rust extensions

Behavior that pi-rust adds on purpose. The parity review should not report these as gaps.

- Account model lists (README "Account model lists"): `openai-codex` from the Codex backend's `/codex/models`, `anthropic` from `/v1/models`, `github-copilot` from the Copilot `/models` endpoint, `openai` and `xai` from `/models`, and `google` from the Gemini API `/models`. Cursor and Grok Build have no public model list, so they are not narrowed.
- Markdown export: `/export` and `pi --export` write Markdown when the output path ends in `.md` (README "Use").
- The README section "Differences from the TypeScript pi" lists the other additions (slash commands, CLI commands and flags, settings) and the bugs pi-rust fixes that are still open upstream. `allowlist.json` holds the matching parity report entries.

## Platform

- `pi experimental server` and `client` use Unix sockets, so they report that they need Unix on Windows.
- The chord2 host manager gives a child its IPC channel as stdin on Unix (upstream uses an extra file descriptor) and as a named pipe on Windows. On Windows, `kill_tree` ends only the child, as upstream does there.
