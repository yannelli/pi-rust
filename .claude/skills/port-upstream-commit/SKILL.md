---
name: port-upstream-commit
description: Port one commit from upstream pi (earendil-works/pi) to pi-rust with tests. Use when the parity report or GAPS.md lists an unported upstream commit, or when asked to port an upstream fix or feature.
---

# Port an upstream commit

Full playbook: `docs/parity/README.md#porting-an-upstream-commit`.

1. `git -C ../pi-upstream show <sha>`. Read the whole diff, including tests and changelog lines.
2. Find the Rust counterpart with the mapping table in `docs/parity/README.md`. Read the Rust code that defines the current behavior before you change it.
3. Port the behavior with the same names, JSON shapes, and messages as upstream. Port the tests to the Rust style. Tests use the `faux` provider or a local mock server.
4. Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and the tests of the crates you changed. Cross-build Windows if you touched `cfg(windows)` code.
5. Commit with a Conventional Commit that names the upstream SHA: `fix(scope): <what> (upstream <short-sha>)`.
6. If this closes an entry in `docs/parity/GAPS.md`, remove the entry in the same change. When every upstream commit up to a point is ported, move `syncedCommit` in `docs/parity/upstream.json`.

If a commit does not apply (browser-only code, a TypeScript build change), add it to `docs/parity/allowlist.json` with the reason.
