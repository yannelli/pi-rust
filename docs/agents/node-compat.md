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
