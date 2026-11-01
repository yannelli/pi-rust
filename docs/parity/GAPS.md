# Open parity gaps

Last reviewed 2026-10-27 against upstream `main` at `531340f10` (2026-10-27). The daily [parity report](README.md#daily-gap-check) finds new gaps. This file keeps the gaps that need a decision or a plan. Remove an entry in the PR that closes the gap.

## Upstream commits not yet ported

None up to `531340f10`. Every commit after the synced commit is ported or listed in `allowlist.json` with a reason.

## Deferred work

- **One copy of the partial message per streamed delta.** Since upstream `f715a15c1`, `pi_ai::streaming_json` parses tool-call arguments once per delta. Each `AssistantMessageEvent` still has a full copy of the partial message. Thus a very large tool call costs more per delta as it grows. A change that removes that copy also changes the public event API.
- **`HarnessOptions.models` as a function** (upstream `7ec1f648e`). This item is not planned. Settings are already a function here, and a `Models` implementation can change its catalog at runtime. The commit is older than the synced commit, so the report does not list it.

## chord2 differences

These are the intentional differences between pi-chord's `host2` and upstream chord:

- Hosts get facets from a `FacetLoader`, because upstream's Node module loader (`loader.ts`) cannot be ported. Thus `7d75a0e3e` does not apply. In that commit, the loader registers every facet root before it imports one. The children of a cluster reload load the named modules through their own `FacetLoader`.
- Factories are synchronous, and `Subscribable::subscribe` cannot fail. A listener's `select` or its resource fails instead.
- `Declare::on_select` takes a key for the identity of `select`. Upstream compares the source text of the function. A reload keeps the subscription while the resource and the key stay the same.
- `Host::host_object` returns the host's own reference to a `lifetime: "host"` object. Upstream tests read the private `hostObjects`.
- Upstream `4a5d37291` moved `OutputBuffer` and `Progress` into `@earendil-works/chord/output`. pi-rust keeps them in pi-durable (`harness/output.rs` and `harness/progress.rs`).
- `Change::append_text` (upstream `appendText`) records an append op and does not compare the strings. But the new string is still a copy. A JSON string is an immutable shared `Arc<str>`, so each append costs O(length) instead of O(text). It takes the path of the object that holds the key, because a Rust draft is a `&mut Json` and not a proxy.
- Durable calls `observe_commits()` observers inside Storage's `adopt`, immediately after the Session's own observers. Upstream (`8b0caefc2`) runs them one microtask later. Rust has no microtask queue, and a spawned task can run after the waits that the commit resolves. A telemetry `add_event` or metric cannot fail in Rust, so there is no `host_error` report for `"telemetry"`.
- Tests: fake timers are real short timers. `Weak` sentinels replace `FinalizationRegistry` with a forced garbage collection (GC). The review test "a commit that still fails midway" is not ported. That test patches the `startTasks` of an object, and Rust code cannot do that.
- The tutorial test (upstream `test/tutorial.test.ts`) is in `chord2_tutorial.rs` and `chord2_tutorial_reload.rs`, with one test for each section. Its drift check compares `docs/tutorial.md` with the TypeScript examples in the test. That check is not ported, because pi-rust has no TypeScript copy of the examples.

## Upstream commits that do not apply

These commits are older than the synced commit, so the report no longer lists them.

- `ea25697e9` managed install markers with a byte order mark (BOM). `pi update --self` replaces the release binary in place (update/install.rs). There is no managed-install.json marker.
- `16f5f0af4` IndexedDbStorage: a browser storage backend. pi-rust has no browser build.
- `c661bc574` drops the shared MCP types block from durable code mode. pi-codemode never had it, so the port of `8bb180bec` already renders the section without it.
- `4c25119f5` replicates durable storages as a commit log, and `cae9fd28a` reverts it. The pair is a net zero change.
- `2685de7a1`, `66c2fa3d7`, and `936e5f40a` update or drop npm dependencies. pi-rust has no npm dependencies.
- `476a303c1` drops an `npm ci` override for managed installs. `pi update --self` downloads release archives and does not use npm.
- `c8ec752cf` skips `fd` and `rg` downloads on unsupported architectures. pi-rust never downloads them, because `grep` and `find` use the `ignore` crate.
- `531340f10` deletes root npm shim files of the TypeScript ai package. pi-rust has no npm package layout.
- The V8 string retention parts of `f715a15c1` (chord `ownedSlice`, bounded error excerpts) do not apply, because Rust strings own their bytes. The rest of the commit is ported.

## Extension runtime (pi-js)

`crates/pi-js/src/unsupported.rs` is the full list. Open items that need a decision:

- **TLS `pfx`** needs a PKCS#12 parser, which is a new dependency. Ask before adding one.
- **TLS before 1.2** cannot work. rustls supports only TLS 1.2 and 1.3.
- **Byte-identical zlib output**: zlib-rs output can have different bytes from Node's zlib output. `memLevel` and `strategy` have no effect.
- **V8-only features** depend on V8 internals that QuickJS does not have. These features are heap snapshots with nodes, `v8.queryObjects`, `inspector.open`, and full `async_hooks` coverage of timers, promises, and I/O.

The Node backend (`extensionRuntime: "node"`, see [Node backend](../agents/node-compat.md#node-backend)) closes these items when it is selected. On the default QuickJS backend they stay open.

- `child_process.fork` and spawning `process.execPath` start a real Node process.
- `process.dlopen` and native `.node` addons load.
- V8-only modules and features work: `v8` heap snapshots and `queryObjects`, `inspector.open`, and full `async_hooks` coverage.

## Extension module exports

Section 8 of the parity report compares the runtime exports of each module in upstream `virtual-modules.ts` with the bundled module. Run `cargo xtask parity --upstream <checkout>` to get the current list. On 2026-10-31 the report found no missing exports.

These exports are present, but they throw `ERR_PI_UNSUPPORTED`:

- `@earendil-works/pi-ai/compat`: `setBedrockProviderModule`. Amazon Bedrock streams in Rust, so there is no Node Bedrock module to inject.
- `@earendil-works/pi-coding-agent`, run modes and CLI: `InteractiveMode`, `RpcClient`, `main`, `parseArgs`, `runPrintMode`, `runRpcMode`, `resolveCliModel`, `resolveModelScopeWithDiagnostics`, `readStoredCredential`.
- `@earendil-works/pi-coding-agent`, sessions and extensions: `AgentSession` and `AgentSessionRuntime` as constructors, `createAgentSessionFromServices`, `createAgentSessionRuntime`, `createAgentSessionServices`, `ExtensionRunner`, `createExtensionRuntime`, `discoverAndLoadExtensions`, `createCodemodeExtension`, `createMcpExtension`, `createToolSearchExtension`, `DefaultPackageManager`.

Other limits of the exports that work:

- `createAgentSession` makes a child session. It supports `cwd`, `agentDir`, `model`, `thinkingLevel`, tool names, `excludeTools`, `noTools`, `systemPrompt`, `resourceLoader`, and the `SessionManager`, `SettingsManager`, and `ModelRuntime` wrappers. These options throw `ERR_PI_UNSUPPORTED`: `customTools`, `scopedModels`, `sessionStartEvent`, a separate custom `ModelRuntime`, third-party session or settings managers, and a custom `ResourceLoader` with skills, prompts, context files, or extensions. A child session does not load extensions. It uses the runtime and auth of the host.
- `AgentSession.prompt` supports images, `expandPromptTemplates`, and `source`. `streamingBehavior` throws `ERR_PI_UNSUPPORTED`. A second prompt during a running prompt is rejected.
- `SDK.extensionsResult.runtime` throws `ERR_PI_UNSUPPORTED`.
- `ProjectTrustStore` uses upstream path, read, and write code with a private synchronous lock port. Unix QuickJS and Node fixtures cover its store, error, lock, and exit behavior. Windows validation, signal cleanup, graceful-fs changes, and lock-map sharing with other host code remain open.
- `SessionManager.appendUsage` throws `ERR_PI_UNSUPPORTED`.
- `SettingsManager` works through `SettingsManager.create` and `SettingsManager.inMemory`. `new SettingsManager()` and `get*`, `set*`, and `remove*` accessors without a Rust counterpart throw `ERR_PI_UNSUPPORTED` with the member name (`host/sdk_storage.js`).
- `DefaultResourceLoader` rejects `extensionFactories`, `additionalExtensionPaths`, `additionalThemePaths`, and every override option except `systemPromptOverride` and `appendSystemPromptOverride`. `getExtensions`, `getSkills`, `getPrompts`, `getThemes`, and `getAgentsFiles` throw unless the matching `no*` option is set.
- `ModelRuntime` and `ModelRegistry` use the runtime of the host session and need a bound session. `ModelRuntime.create` with options, login, credentials, and provider registration throw `ERR_PI_UNSUPPORTED`. These reads work: `getModel`, `getModels`, `getAllModels`, `getAllAvailable`, `getAvailableSnapshot`, `getProviders`.
- `generateDiffString` and `generateUnifiedPatch` use a fixed value of 4 context lines.
- `createModels` and `createProvider` are upstream code. The providers of `@earendil-works/pi-ai/providers/all` read the Rust catalog and stream, classify, and generate images in Rust. Their API key auth resolves as the Rust resolver does, over the credential variables that `ctx.env` of upstream `ApiKeyAuth.resolve` returns. Google Vertex AI asks `ctx.fileExists` for its application default credentials file, as upstream does. Their API key `login` asks for one key. For Amazon Bedrock, Google Vertex AI, and Cloudflare, upstream asks for more values. OAuth `login`, `refresh`, and `toAuth` run the Rust flows.
- `ExtensionEditorComponent` runs the external editor through the Rust editor runner. An editor ended by a signal reports `signal <number>` where upstream names the signal. In print and RPC modes the external editor fails with a message, because pi has no terminal there.
- `builtinProviders()` returns the 42 upstream providers. `getBuiltinProviders()` also lists `cursor` and `google-antigravity`, which only pi-rust has.

## Removed upstream, still in pi-rust

Upstream `7472cda0d` (2026-10-12) deleted the experimental server stack: the client, protocol, and server packages and `pi experimental`. A decision on 2026-10-21 keeps these items in pi-rust on purpose as pi-rust extras:

- `pi-client`, `pi-protocol`, and `pi-server`
- the `pi experimental` commands (`server`, `client`, `durable`, `vacation`)
- the facets and services runtime in pi-chord that they use

The three crates are in `allowlist.json`. They get no upstream fixes from now on.

Upstream `92ab53602` removed the Chord state getters `Session.documentState()`, `Conversation.viewState()`, and `Harness.taskGraph()` with the `DocumentState` type. pi-durable keeps them because the experimental stack in pi-coding-agent reads conversations and the task graph through them. When a document is retired, its document state publishes `null`. When a new document is created at the same address, the state publishes the value of that document. A watch on an address works the same way.

## pi-rust extensions

pi-rust adds this behavior on purpose. Do not report these items as gaps in the parity review.

- Account model lists ([Account model lists](../../crates/pi-coding-agent/docs/providers.md#account-model-lists)). Each provider gets its list from one endpoint:
  - `openai-codex` from the Codex backend's `/codex/models`
  - `anthropic` from `/v1/models`
  - `github-copilot` from the Copilot `/models` endpoint
  - `openai` and `xai` from `/models`
  - `google` from the Gemini API `/models`
  - `cursor` from `AvailableModels`
  - `google-antigravity` from the `model` and `thought_level` selectors of the Antigravity ACP server
- Subscription providers `cursor` and `google-antigravity` with their logins and the APIs `cursor-agent` and `antigravity-acp` ([Subscription providers](../providers/subscription-providers.md)).
- Fast mode: `/fast`, `--fast`, the `fastMode` setting, `compat.fastMode`, the ACP `fast_mode` option, RPC `set_fast_mode`, and the `fastMode` and `fastModeAvailable` fields of `get_state` ([Fast mode](../../crates/pi-coding-agent/docs/providers.md#fast-mode-pi-rust-extension)).
- Markdown export: if the output path ends in `.md`, `/export` and `pi --export` write Markdown ([Export](../../crates/pi-coding-agent/docs/commands.md#export)).
- The README section "Differences from upstream pi" lists the other additions: slash commands, CLI commands and flags, and settings. It also lists the bugs that pi-rust fixes and that are still open upstream. `allowlist.json` holds the matching parity report entries.

## Post-release follow-up

Do this item only after the preceding ACP, provider, and TUI work is released on `main`. Then find why Anthropic Sonnet 4.5 has the label `(Latest)`. Change the label or its update source only if Pi Rust supplies it. Do not override metadata that an external provider supplies. Do not investigate this item before that release.

## ACP and provider integration work

- Paseo rejects exact MCP tool preapproval for generic ACP providers before it starts Pi. Its provider contract and ACP permission handler must match tools exactly. Pi's permission modes alone cannot resolve that host-side error.
- ACP `session/new` and `session/load` do not load client-supplied `mcpServers`. Pi's own MCP configuration works, but a separate integration is necessary to forward host MCP definitions.
- ACP has no standard model icon field. Fast catalog variants get a lightning-bolt label. Models with a fast mode get the **Speed** option ([Fast mode](../../crates/pi-coding-agent/docs/providers.md#fast-mode-pi-rust-extension)).

## Platform

- `pi experimental server` and `client` use Unix sockets. On Windows, they report that they need Unix.
- On Unix, the chord2 host manager gives a child its IPC channel as stdin. Upstream uses an extra file descriptor there. On Windows, the channel is a named pipe. On Windows, `kill_tree` ends only the child, as upstream does there.
- When pi-durable stops waiting for a local shell command's output (100 ms after the exit or the last chunk after it), it delivers queued chunks and any chunk the readers finish reading before they observe the stop signal. On Unix it also takes at most 64 KiB more per stream if readable without waiting. Windows pipes read on a blocking thread, so on Windows it starts no further read.
- This Rust tree has no Durable Objects or workerd runtime target. `sqlite_cloudflare.rs` checks the Durable Object SQLite driver's contract, including the `SELECT changes()` count of `run_counted`, on a stand-in adapter over the bundled SQLite. Upstream's workerd tests (`test-workers/`) are not run.
