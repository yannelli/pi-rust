# Node compatibility in pi-js

Extensions run on pi-js, an embedded QuickJS runtime with Node 22 built-in modules. Upstream pi runs extensions on real Node. Thus each Node API that an extension can call is part of parity.

## Layout

| What | Where |
| --- | --- |
| Native code for a module | `crates/pi-js/src/node/`. Usually `<module>.rs`. Large modules add files such as `net_server.rs` or `tls_wrap.rs`. `crypto/` is a directory, and `worker_threads` is in `worker.rs`. |
| JavaScript for a module | `crates/pi-js/src/js/<module>.js` |
| Modules `require` can load | `NODE_BUILTINS` in `crates/pi-js/src/resolve/mod.rs` |
| Supported module list | `crates/pi-js/src/node/builtins.rs` |
| APIs that throw `ERR_PI_UNSUPPORTED` | `UNSUPPORTED_APIS` in `crates/pi-js/src/unsupported.rs` |
| APIs that work but are different from Node | `DIFFERENCES` in the same file |
| Tests | `crates/pi-js/tests/node_*.rs` |

## Node backend

The Node backend runs extensions on a real `node` process in place of QuickJS. Use it for extensions that need native addons, `child_process.fork`, or V8-only modules. The code is in `crates/pi-js/src/node_backend/`.

### Select the backend

| Where | Value |
| --- | --- |
| CLI | `--extension-runtime quickjs\|node\|auto` |
| Environment | `PI_EXTENSION_RUNTIME=quickjs\|node\|auto` |
| Settings | `"extensionRuntime": "quickjs"`, `"node"`, or `"auto"` in project or global `settings.json` |

- The order is CLI, then environment, then project settings, then global settings. The default is `quickjs`.
- `auto` uses Node when a loaded extension package sets `package.json` `pi.runtime: "node"`, or ships a `.node` file within 4 directory levels of its root. Otherwise `auto` uses QuickJS.
- pi finds `node` in `PI_NODE_PATH`, then on `PATH`. The minimum version is Node 22.
- If Node is missing, too old, or does not start, pi shows one warning and uses QuickJS.
- The code that selects the backend is in `crates/pi-coding-agent/src/extensions/backend.rs`.

### How it works

- Extension code runs in a `worker_threads` Worker inside the `node` process. `JsHandle::interrupt` ends the worker.
- Module hooks resolve `pi` virtual modules and TypeScript files. On Node 22.15+ and 23.5+, pi uses `module.registerHooks`. On older Node 22 releases, pi uses `module.register` for ESM and a `Module._load` patch for CommonJS.
- TypeScript goes through the oxc transpiler of pi-js, as on QuickJS.
- The closure APIs `JsHandle::with`, `JsHandle::blocking_with`, and `JsHandle::post` return an error on Node, because the engine is in another process. Use the host function and `JsRef` APIs.
- pi writes the bootstrap scripts (`crates/pi-js/src/node_backend/js/`) to `node-<hash>` under the `cache_dir` option and starts `node` with them. pi-coding-agent sets `cache_dir` to `<agentDir>/cache/pi-js-node` for the Node backend. pi creates both directories with mode 700 and refuses them when another user owns them or their permissions are wider. With no `cache_dir`, the scripts go to a new temporary directory that pi removes at shutdown.
- Protocol messages go over a loopback TCP connection (`transport.rs`), one JSON message per line. Rust listens on `127.0.0.1:0` and passes the port and a random 32-byte hex token in `PI_NODE_BACKEND_PORT` and `PI_NODE_BACKEND_TOKEN`. `main.cjs` removes both from `process.env`, connects, and sends the token as its first line. Rust accepts the first connection with the correct token, compares tokens in constant time, and closes the listener.
- The stdin of `node` is `/dev/null`. Its stdout and stderr go to the runtime's `OutputSink` as raw bytes, so output from children spawned with `stdio: "inherit"`, `fs.writeSync(1, ...)`, and native addons reaches the sink, as on QuickJS with a non-process sink. Children spawned with `stdio: "inherit"` see a pipe, also when the sink is `StdOutput`. Node resets the terminal mode it saw at startup when it exits, so pi does not give it the terminal.
- Shutdown ends the socket, waits up to 2 s for `node`, then kills it. The stdout and stderr forwarder threads are detached, so a grandchild that holds them open does not delay shutdown. Waiting calls check every 250 ms that `node` still runs, and fail with `ERR_PI_NODE_EXITED` when it is gone.
- A sync host call from the worker waits on a `SharedArrayBuffer` with `Atomics.wait` until the reply comes back. Sync host functions run on the Rust bridge thread. A blocking `JsRef` call from that thread fails, as it does on the QuickJS thread.
- A `JsRef` is an id in a table in the worker. Dropping the last clone in Rust releases it.
- `process.exit` in an extension calls the `on_exit` callback and ends the worker. If `node` exits, pending calls fail with `ERR_PI_NODE_EXITED`.
- Stack traces show positions in the transpiled code. They are not mapped back to the TypeScript source.

Version-specific workarounds in `worker_modules.cjs` and `hooks.mjs`:

- Node 22.x with `module.registerHooks`: a CommonJS file that `import()` loads gets a `require` that cannot load ES modules (`internal/modules/esm/translators`, `loadCJSModule`). The load hook returns the internal format `"commonjs-sync"` for those files, so the CommonJS loader runs them. This depends on a Node internal name. Check it when a new Node 22 release breaks the tests. Node 23 and later do not need it.
- Node 22.0 to 22.14 (`module.register`): Node reads re-exported CommonJS files from disk to list their exports, and registered sources are not on disk. The load hook writes `require(x)` as `require.call(void 0, x)` in CommonJS that Rust supplies, so the export scan skips those calls. `require_rewrite.mjs` does the rewrite and skips strings, template text, comments, and regular expression literals. It finds a regular expression literal from the token before the `/`; a `/` after `)` counts as division.
- Node 22.0 to 22.14: the `MessagePort` of the hooks thread must stay referenced. With `port.unref()`, replies do not wake the hooks thread and imports hang.

### Test on Node

Set `PI_EXTENSION_RUNTIME=node` to run the extension tests on Node. The harness in `crates/pi-coding-agent/tests/extensions/harness.rs` then fails a test if the runner fell back to QuickJS.

```sh
PI_EXTENSION_RUNTIME=node cargo test -p pi-coding-agent --test extensions --no-fail-fast
cargo test -p pi-js --test node_backend --test node_backend_modules --test node_backend_transport
```

The pi-js Node tests skip with a message when no Node 22 or later is found. Set `PI_NODE_PATH` to test one Node version, for example Node 22.14 for the `module.register` path.

## Close a gap

1. Pick an entry from `UNSUPPORTED_APIS` or `DIFFERENCES`. Read how Node 22 behaves. Include the error codes and messages.
2. Use the structure of a module that already works (for example `net`, `zlib`, or `worker_threads`). Keep new files under 480 lines.
3. Remove or reword the entry in `unsupported.rs` in the same change.
4. Add a test that compares the result with the Node 22 output. To get the expected JSON, run the script under real Node 22. Paste the JSON into the test.
5. Do not add a crate before you ask. You can use a crate that is already in `Cargo.lock`, or a feature of an existing dependency. It must build for every release target.

## Test rules

- Use only localhost. Get temporary directories from `tempdir()`. Do not use real AI providers.
- Do not depend on tight timing. Wait for events. Do not sleep for a fixed delay.
- If a script can hang, wrap it in `run_cjs_within`. Record each step in a `stage` file ([CI triage](ci-triage.md#hangs-and-flaky-tests)).
- Windows is different for paths, named pipes, and the transfer of handles. Skip a Windows-only difference as the other tests do. Give the reason in the skip message.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p pi-js --locked --no-fail-fast
cargo zigbuild --target x86_64-pc-windows-gnu -p pi-js --tests
cargo zigbuild --target armv7-unknown-linux-musleabihf -p pi-js
```
