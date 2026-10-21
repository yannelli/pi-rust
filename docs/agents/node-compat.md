# Node compatibility in pi-js

Extensions run on pi-js, an embedded QuickJS runtime with Node 22 built-in modules. Upstream pi runs extensions on real Node, so every Node API an extension can call is part of parity.

## Layout

| What | Where |
| --- | --- |
| Native code for a module | `crates/pi-js/src/node/`: usually `<module>.rs`, plus files such as `net_server.rs` or `tls_wrap.rs` for large modules; `crypto/` is a directory and `worker_threads` is `worker.rs` |
| JavaScript for a module | `crates/pi-js/src/js/<module>.js` |
| Modules `require` can load | `NODE_BUILTINS` in `crates/pi-js/src/resolve/mod.rs` |
| Supported module list | `crates/pi-js/src/node/builtins.rs` |
| APIs that throw `ERR_PI_UNSUPPORTED` | `UNSUPPORTED_APIS` in `crates/pi-js/src/unsupported.rs` |
| APIs that work but differ from Node | `DIFFERENCES` in the same file |
| Tests | `crates/pi-js/tests/node_*.rs` |

## Closing a gap

1. Pick an entry from `UNSUPPORTED_APIS` or `DIFFERENCES`. Read how Node 22 behaves, including error codes and messages.
2. Follow the structure of a module that already works (for example `net`, `zlib`, or `worker_threads`). Keep new files under 480 lines.
3. Remove or reword the entry in `unsupported.rs` in the same change.
4. Add a test that compares with Node 22 output. Run the script under real Node 22 to get the expected JSON, then paste it into the test.
5. Do not add a crate without asking. A crate already in `Cargo.lock`, or a feature of an existing dependency, is fine if it builds for every release target.

## Test rules

- Localhost only, temp dirs from `tempdir()`, and no real AI providers.
- No tight timing: wait for events instead of sleeping for a fixed delay.
- Wrap scripts that could hang in `run_cjs_within` and mark progress with a `stage` file ([CI triage](ci-triage.md#hangs-and-flaky-tests)).
- Windows differs in paths, named pipes, and handle passing. Skip a Windows-only difference the way the other tests do, and say why in the skip message.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p pi-js --locked --no-fail-fast
cargo zigbuild --target x86_64-pc-windows-gnu -p pi-js --tests
cargo zigbuild --target armv7-unknown-linux-musleabihf -p pi-js
```
