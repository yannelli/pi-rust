---
name: node-compat-gap
description: Close a Node 22 API gap in pi-js, the QuickJS runtime that runs pi extensions. Use when an extension needs a Node API that throws ERR_PI_UNSUPPORTED or behaves differently, or when working on entries in crates/pi-js/src/unsupported.rs.
---

# Close a Node API gap

Full playbook: `docs/agents/node-compat.md`. Rules for the crate: `crates/pi-js/AGENTS.md`.

1. Find the entry in `crates/pi-js/src/unsupported.rs` (`UNSUPPORTED_APIS` or `DIFFERENCES`).
2. Read how Node 22 behaves. Write a small script and run it under real Node 22 to get the exact output and error codes.
3. Implement it in `crates/pi-js/src/node/<module>.rs` and `crates/pi-js/src/js/<module>.js`, following a module that already works.
4. Update the `unsupported.rs` entry in the same change.
5. Add a test in `crates/pi-js/tests/` that compares with the Node output. Use `run_cjs_within` for anything that can hang.
6. Run the checks listed at the end of `docs/agents/node-compat.md`, including the Windows and armv7 cross-builds.

Ask before adding a crate. A crate already in `Cargo.lock`, or a feature of an existing dependency, is fine when it builds for every release target.
