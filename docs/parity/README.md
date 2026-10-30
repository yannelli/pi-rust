# Parity playbook

pi-rust ports each feature of the TypeScript pi monorepo ([earendil-works/pi](https://github.com/earendil-works/pi)). This playbook tells you what to compare, how to find gaps, and how to close them. People and coding agents (Claude Code, Codex, Cursor) use the same steps.

## Sources of truth

| What | Where |
| --- | --- |
| Upstream code | `earendil-works/pi`, branch `main`, `packages/*` |
| Upstream commit that this port is synced to | [`upstream.json`](upstream.json) (`syncedCommit`) |
| Accepted differences | [`allowlist.json`](allowlist.json), each with a reason |
| Open gaps | [`GAPS.md`](GAPS.md) |
| Node API gaps in the extension runtime | `crates/pi-js/src/unsupported.rs` (`UNSUPPORTED_APIS`, `DIFFERENCES`) |
| User-facing differences | "Differences from upstream pi" in the [README](../../README.md) |

Compatibility rule: where upstream defines these items, they must match upstream byte for byte. The items are JSON field names, session files, settings keys, config paths, RPC and JSON event shapes, and the HTML export.

## Daily gap check

The [Parity workflow](../../.github/workflows/parity.yml) runs every day and on demand. It checks out upstream, generates the upstream model data, and runs the report. Then it adds CI health for `main` and updates the open issue with the label `parity-report`.

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

The report cannot see these items. When an upstream commit changes them, check them by hand: TUI rendering, keybindings, themes, the system prompt text, compaction prompts, provider request bodies, and docs.

## Triage

For each finding:

1. Confirm it in both codebases. A missing name can be a rename or a different file layout.
2. Do one of these:
   - Port it.
   - Record it as accepted in `allowlist.json` with a reason.
   - List it in `GAPS.md` with what blocks it.
3. Put one upstream commit or one gap in each change. A large item (for example the chord2 runtime) gets its own branch and PR.

To fix a model catalog finding, do not edit `crates/pi-ai/models.json` by hand. Regenerate it from the data of a Parity run:

```sh
gh run download <run-id> -R yannelli/pi-rust -n upstream-model-data -D ../model-data
cargo xtask models ../model-data
```

## Porting an upstream commit

1. Read the upstream diff: `git -C ../pi-upstream show <sha>`.
2. Use the table above to find the Rust counterpart. Keep names, JSON shapes, and error messages the same as upstream.
3. Port the tests too. Tests use the `faux` provider or a local mock server. Tests do not use real provider keys or the network.
4. Write `upstream <sha>` with the 9-character SHA in the commit message, for example `fix(ai): recover from Copilot request-size overflow (upstream 73f8de80b)`. The report marks a commit as ported only when it finds that text.
5. If every commit up to an upstream commit is ported or allowlisted, update `upstream.json`. Move `syncedCommit`, `syncedVersion`, and `syncedOn` to that commit.

## Verification

Before a PR, run the repository checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
```

CI runs them on Linux, macOS, and Windows. If the code has `cfg(windows)` paths and `cargo-zigbuild` is installed, also cross-build locally: `cargo zigbuild --target x86_64-pc-windows-gnu --workspace --tests`. For Windows failures, refer to [the CI triage guide](../agents/ci-triage.md).

## Release

A merge to `main` starts an automatic release. After CI passes, the Release workflow runs `cargo xtask plan`. The workflow bumps the version from the Conventional Commits and adds a tag. Then it builds every target and publishes the GitHub release. Refer to [the release guide](../agents/release.md).
