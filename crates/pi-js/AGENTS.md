# pi-js rules

pi-js runs pi extensions on QuickJS with Node 22 built-in modules. Read [docs/agents/node-compat.md](../../docs/agents/node-compat.md) before you change it.

- Match Node 22: return values, error codes, error messages, and event order. Compare with real Node output in tests.
- Keep `src/unsupported.rs` true. Remove or reword an entry in the change that closes it, and add one for any behavior you leave partial.
- A module is native code in `src/node/<module>.rs` plus JavaScript in `src/js/<module>.js`. Keep each new file under 480 lines.
- Tests use localhost and temp dirs only. Wrap scripts that could hang in `run_cjs_within` and mark progress in a `stage` file.
- `cfg(windows)` code must cross-build: `cargo zigbuild --target x86_64-pc-windows-gnu -p pi-js --tests`.
