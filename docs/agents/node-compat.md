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
- pi writes the bootstrap scripts (`crates/pi-js/src/node_backend/js/`) to the cache directory and starts `node` with them. The main thread of `node` sends one JSON message per line on stdout and reads them on stdin.
- A sync host call from the worker waits on a `SharedArrayBuffer` with `Atomics.wait` until the reply comes back. Sync host functions run on the Rust bridge thread. A blocking `JsRef` call from that thread fails, as it does on the QuickJS thread.
- A `JsRef` is an id in a table in the worker. Dropping the last clone in Rust releases it.
- `process.exit` in an extension calls the `on_exit` callback and ends the worker. If `node` exits, pending calls fail with `ERR_PI_NODE_EXITED`.
- Stack traces show positions in the transpiled code. They are not mapped back to the TypeScript source.

### Test on Node

Set `PI_EXTENSION_RUNTIME=node` to run the extension tests on Node. The harness in `crates/pi-coding-agent/tests/extensions/harness.rs` then fails a test if the runner fell back to QuickJS.

```sh
PI_EXTENSION_RUNTIME=node cargo test -p pi-coding-agent --test extensions --no-fail-fast
cargo test -p pi-js --test node_backend --test node_backend_modules
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
