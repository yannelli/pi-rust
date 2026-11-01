# SettingsManager oracle prep (source only, not run)

Created 2026-11-01 by T17, revised the same day. Nothing in this folder has been executed. It runs the authentic upstream `SettingsManager` of earendil-works/pi `35dfa757378bd7a8570670e99c2687bdd1039a06`, with its lock-pinned dependencies, and records what it does. It is no rewrite and uses no stubs. Every expected value comes from that run.

## Sources and provenance
- **Upstream source** (`upstream/`): exact blobs of the import closure of `packages/coding-agent/src/core/settings-manager.ts`:
  - `config.ts`, `utils/{paths,text,child-process}.ts`;
  - `core/{settings-defaults,http-dispatcher}.ts`;
  - `packages/ai/src/utils/retry.ts`;
  - both `package.json` files;
  - `settings-schema.ts`, a reference only (imported as types).
  
  `data/upstream-manifest.json` holds each blob id, sha256 and size.
  - **Where they came from:** the first prep read them with `git cat-file` from the local reference checkout `/Users/ryanyannelli/Development/pi-upstream-ref/pi`. That checkout's HEAD is `f09d01b` (a later upstream commit), but its object store holds 35dfa757, so `35dfa757:<path>` gives the pinned blobs. It has no `node_modules`, which is why the first prep found no packages.
  - **Protected copies:** root's copies are `/tmp/t17-upstream-git-35dfa` (git, HEAD = 35dfa757, clean, `node_modules` → symlink to `/tmp/t17-upstream/node_modules`) and `/tmp/t17-upstream` (35dfa export with the installed packages). All 11 prep files are byte-identical to `/tmp/t17-upstream` (checked).
  - **From now on:** `prepare` verifies against the protected git copy, requiring HEAD = 35dfa757, clean status and an unchanged link.
- **Packages** (`node_modules/`): copied once, read-only, from `/tmp/t17-upstream/node_modules`. The fingerprint (size, mtime, sha256) of every source file was equal before and after the copy.
  - The closure follows the pinned `package-lock.json` (35dfa757, recorded in `data/packages-manifest.json`) from `packages/coding-agent`'s dependencies. There are 11 packages and 292 files:
    - `proper-lockfile` 4.1.2 (with its nested `retry` 0.12.0), `graceful-fs` 4.2.11, `signal-exit` 3.0.7;
    - `undici` 8.12.3;
    - `cross-spawn` 7.0.6, `path-key` 3.1.1, `shebang-command` 2.0.0, `shebang-regex` 3.0.0, `which` 2.0.2, `isexe` 2.0.0.
    
    The pinned lockfile has no nested workspace copies that would shadow them.
  - **Per package:** the lock version equals the installed `package.json` version, and the lock `integrity` equals the installed hidden lockfile entry (`node_modules/.package-lock.json`, sha256 in the manifest). The tarball itself is not available, so file bytes are tied to the cache by sha256 and to the lock by version and integrity record.
- **One deliberate resolution choice:** `@earendil-works/pi-ai/utils/retry` resolves through the workspace link to `packages/ai`, whose `exports` name the built `dist/utils/*.js`. That build output is not in git. The hook maps this one specifier to the git-pinned `packages/ai/src/utils/retry.ts` instead. It is the only module not resolved by Node itself.

## Isolation and run-time verification
- **Run copy:** `prepare` checks every source three ways (manifest, sha256, and the protected 35dfa tree entry) and every package file against the manifest, both in prep and in the cache. It then copies `upstream/` and `node_modules/` into a new run directory and re-hashes the copy.
- **Resolution:** the hooks resolve only Node builtins, the run copy and this prep folder, and refuse anything else. Every ESM-loaded run-copy file must match a manifest and is logged to `loaded.jsonl`.
- **CommonJS:** `require()` inside the packages bypasses the hooks, so `run.mjs` lists `Module._cache` at the end. It fails on any path outside the run copy. `verify` requires every listed file to be a manifest package file, and the whole run copy to be byte-unchanged after the run.
- **Environment:** `HOME` points into the run directory; `TZ=UTC`, `LANG=C`. `VISUAL`, `EDITOR`, `PI_CLEAR_ON_SHRINK`, `PI_HARDWARE_CURSOR` and `PI_CODING_AGENT_DIR` are cleared per case and set only where a case says so. No real settings, auth or stores are read.
- **Determinism:** UUIDs become `<uuid#n>`; run, home and copy paths become `<RUN>`, `<HOME>` and `<COPY>`. Lock folder mtimes are not recorded. Two runs must be byte-identical.

## Timing semantics of the harness
- **Synchronous calls:** a member call runs synchronously, and its result is copied at once. Only a returned thenable is awaited: `flush`, `reload`, a rejected promise.
  - So upstream's queued write (`enqueueWrite`, a promise chain, `settings-manager.ts:553-564`, used by `save`) cannot run between steps unless a step yields: `$yield` (one `await`), `flush` or `reload`.
- **`$assertNoWrite <label>`:** fails the whole run if storage was written since the case started or since the last snapshot. Written means a recording-storage log entry with a `next` text, or a changed file tree for file cases. It runs:
  - before every pre-flush snapshot of the setter cases;
  - before and after each external edit;
  - before `flush` in the file setter and queue cases;
  - in the child API contract.
  
  The `queue/yield-lets-queued-write-run` and `file/yield-lets-queued-write-run` cases record, without asserting, when the queued write lands after one and after four `$yield`s.
- **Consuming snapshots:** `$snapshot` drains `drainErrors()` and the storage log, and moves the file baseline of `$assertNoWrite` forward. Each snapshot carries `consumed: ["drainErrors", "storageLog", "fileBaseline"]`. Errors appear in the first snapshot (or `drainErrors` step) after they occur. The final snapshot follows an unrecorded `flush()`.

## Coverage
- 139/139 public members (`data/public-members.json`, scanned from the class body).
- A member counts only when a case called it through the recorder. Snapshots and final reads use the unrecorded manager. `run.mjs` and `verify` fail unless every member has at least 2 distinct cases.
- `data/coverage-plan.json` lists the planned case ids per member (at least 2 each; `create` 79).
- **Getters:** `empty`, `global` (every key, plus an unknown key), `layered` (with project overrides and tool deltas) and `invalid`. The env getters also run with `ENV_SET` and `ENV_ALT`, and object/array getters get mutate-then-read cases.
- **Setters:** each runs:
  - every argument variant (clamps, throws, `undefined` clears) with read-back, before and after `flush`, and the full custom `withLock` log;
  - an external edit before `flush`;
  - in memory;
  - **through real files** (`create` with seeded global and project `settings.json`, then `flush` and the file tree);
  - untrusted, for project setters.
- **Lifecycle (36):** the in-memory and custom-storage factories, malformed storage, migration, raw layers and copies, trust, reload, overrides, queue order and write and load errors, device and tracking ids, and the child API contract.
- **File cases (21, `data/file-cases.mjs`):**
  - empty dirs; untrusted; reading existing files;
  - writes that create `agent/` or `cwd/.pi/`; an untrusted project write;
  - an external edit before `flush`; BOM; a legacy file;
  - malformed global and project files; `settings.json` that is a directory;
  - a held lock on read and on write (`<file>.lock` folder, real proper-lockfile retries); a stale lock taken over (folder aged 3600 s);
  - reload after an external edit; trust false→true reading the file;
  - the default agent dir from `PI_CODING_AGENT_DIR`; the device id persisted.
- **Child API contract** (`lifecycle/child-api-contract/agent-session-writes`): the 9 SettingsManager calls of upstream `agent-session.ts` (blob dbe36c3658a3), interleaved with a caller setter and an external edit. It is the manager side only; which AgentSession API triggers each call is a separate contract.

## Future run (needs a dedicated Node grant; not granted)
```
python3 -I /tmp/t17-sdk-settings-oracle-prep/oracle/prepare_run.py prepare /tmp/t17-sdk-settings-oracle-run1
# twice, N = 1 and 2, with the exact env/argv/cwd stored in run-inputs.json "commands" (timeout 120 s each):
#   env -i <env> /Users/ryanyannelli/.nvm/versions/node/v22.23.3/bin/node --import <prep>/oracle/register.mjs \
#     <prep>/oracle/run.mjs <run>/out-N/work <run>/out-N/golden.json
python3 -I /tmp/t17-sdk-settings-oracle-prep/oracle/prepare_run.py verify /tmp/t17-sdk-settings-oracle-run1
```
- Total budget 5 minutes, stop at the first failure, no retry.
- Assumption (I): Node v22.23.3 strips TypeScript by default (since v22.18). The runtime `.ts` files have no enums, namespaces, parameter properties or `import =` (static scan). If stripping is off, the run fails at load.
- Held-lock cases spend about 200 ms each in proper-lockfile's 10×20 ms loop.
- The goldens and generator move into the repo only in the implementation step, with their sha256.

## Contract decisions recorded for implementation (design only, no code yet)
- **Custom storage lifetime:** a `fromStorage` storage object stays referenced for the extension runtime's lifetime, held by the JS wrapper and by the registered drain callback. It is not released on JS garbage collection, because a child session can still hold the shared Rust manager, and its writes must reach that storage, as upstream reachability would allow. It is released only at runtime shutdown or by an explicit internal dispose.
- **Schema diagnostics:** upstream reports only storage and JSON errors through `drainErrors()` (`collectSettingsDiagnostics` = `drainErrors`), with no run-time schema validation.
  - The SDK `SettingsManager.drainErrors()` must return exactly those.
  - pi-rust's schema-key messages (`settings/manager.rs:198-210`) are a pi-rust CLI extra. For CLI startup diagnostics (`services.rs:213`, `main.rs:461`, `doctor/mod.rs:172`) they go in a separate list; the after-action `ensure!` callers (`extensions_view_actions.rs:73`, `moved_features.rs:19`) and `packages/cli_support.rs:187` check storage errors only.
  - Proposed; it needs approval before any code change.

## Remaining blockers
1. **No Node grant yet** for the two oracle runs.
2. **Platform:** the oracle is macOS only. Windows branches (`getExternalEditorCommand` falls back to `notepad`, `settings-manager.ts:983`; `normalizePath`; the legacy `managed_config.toml` path) need a Windows run, or source-derived cases.
3. **ELOCKED message:** proper-lockfile's held-lock message is now recorded by the oracle. Rust `FileLock` reports `WouldBlock` "<file> is locked", so matching or documenting it is a decision for implementation.
4. **retry source mapping:** see the resolution choice above. If root prefers the built `dist` file, it needs its own provenance (not in git).
