# Parity playbook

pi-rust ports the TypeScript pi monorepo ([earendil-works/pi](https://github.com/earendil-works/pi)) feature for feature. This playbook says what to compare, how to find gaps, and how to close them. People and coding agents (Claude Code, Codex, Cursor) follow the same steps.

## Sources of truth

| What | Where |
| --- | --- |
| Upstream code | `earendil-works/pi`, branch `main`, `packages/*` |
| Upstream commit this port has caught up to | [`upstream.json`](upstream.json) (`syncedCommit`) |
| Accepted differences | [`allowlist.json`](allowlist.json), each with a reason |
| Open gaps | [`GAPS.md`](GAPS.md) |
| Node API gaps in the extension runtime | `crates/pi-js/src/unsupported.rs` (`UNSUPPORTED_APIS`, `DIFFERENCES`) |
| User-facing differences | "Differences from upstream pi" in the [README](../../README.md) |

Compatibility rules: JSON field names, session files, settings keys, config paths, RPC and JSON event shapes, and the HTML export must match upstream byte for byte where upstream defines them.

## Daily gap check

The [Parity workflow](../../.github/workflows/parity.yml) runs every day and on demand. It checks out upstream, generates the upstream model data, runs the report, adds CI health for `main`, and updates the open issue labeled `parity-report`.

Run the same report locally:

```sh
git clone https://github.com/earendil-works/pi ../pi-upstream   # once
git -C ../pi-upstream pull --ff-only
(cd ../pi-upstream && npm ci --ignore-scripts && npm run hydrate-model-data -w packages/ai)   # model catalog only
cargo xtask parity --upstream ../pi-upstream --out parity-report.md
```

`--strict` exits with status 1 when a finding is not in the allowlist. The report is a text comparison, so check each finding in the code before you act on it.

| Section | Upstream | pi-rust |
| --- | --- | --- |
| Commits after `syncedCommit` | `git log` on `packages/` | commit messages that name the upstream short SHA |
| CLI flags | `packages/coding-agent/src/cli/args.ts` | `crates/pi-coding-agent/src/cli.rs`, `cli_help.rs` |
| Settings keys | `packages/coding-agent/src/core/settings-schema.ts` | `crates/pi-coding-agent/src/settings/schema.rs` |
| Slash commands | `packages/coding-agent/src/core/slash-commands.ts` | `crates/pi-coding-agent/src/interactive/` |
| Built-in tools | `packages/coding-agent/src/core/tools/` | `crates/pi-coding-agent/src/tools/` |
| Model catalog | `packages/ai/src/providers/data/*.json` (generated, not in git) | `crates/pi-ai/models.json` |
| Extension API | `ExtensionAPI` in `packages/coding-agent/src/core/extensions/types.ts` | `crates/pi-coding-agent/src/extensions/` |
| Node built-ins | `require("module").builtinModules` (Node 22) | `crates/pi-js/src/resolve/mod.rs`, `unsupported.rs` |
| Packages | `packages/*` | `crates/pi-*` |

Things the report cannot see, to check by hand when an upstream commit touches them: TUI rendering, keybindings, themes, the system prompt text, compaction prompts, provider request bodies, and docs.

## Triage

For each finding:

1. Confirm it in both codebases. A missing name can be a rename or a different file layout.
2. Decide: port it, record it as accepted in `allowlist.json` with a reason, or list it in `GAPS.md` with what blocks it.
3. One upstream commit or one gap per change. Large items (for example the chord2 runtime) get their own branch and PR.

Model catalog findings are fixed by regenerating `crates/pi-ai/models.json` from the data of a Parity run, not by hand:

```sh
gh run download <run-id> -R yannelli/pi-rust -n upstream-model-data -D ../model-data
cargo xtask models ../model-data
```

## Porting an upstream commit

1. Read the upstream diff: `git -C ../pi-upstream show <sha>`.
2. Find the Rust counterpart with the table above. Keep names, JSON shapes, and error messages the same as upstream.
3. Port the tests too. Tests use the `faux` provider or a local mock server; never real provider keys or network.
4. Write `upstream <sha>` with the 9-character SHA in the commit message, for example `fix(ai): recover from Copilot request-size overflow (upstream 73f8de80b)`. The report marks a commit as ported only on that text.
5. When every commit up to an upstream commit is ported or allowlisted, move `syncedCommit`, `syncedVersion`, and `syncedOn` in `upstream.json` to it.

## Verification

Before a PR, run the repository checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
```

CI runs them on Linux, macOS, and Windows. Code with `cfg(windows)` paths should also cross-build locally with `cargo zigbuild --target x86_64-pc-windows-gnu --workspace --tests` when `cargo-zigbuild` is installed. See [the CI triage guide](../agents/ci-triage.md) for Windows failures.

## Release

Merging to `main` releases automatically: CI passes, the Release workflow runs `cargo xtask plan`, bumps the version from the Conventional Commits, tags, builds every target, and publishes the GitHub release. See [the release guide](../agents/release.md).
