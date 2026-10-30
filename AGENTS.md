# Development Rules

- Rust-only port of the TypeScript pi harness. Keep JSON field names, session files, and config paths compatible with upstream pi.
- After code changes run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and the tests of the crates you changed. Before a PR run `cargo test --workspace --locked --no-fail-fast`. Fix every warning.
- Tests must not use real provider APIs or keys: use `pi_ai::providers::faux` or a local mock HTTP server.
- Providers emit events only through `pi_ai::MessageBuilder`, and every stream must end with `Done` or `Error`.
- Keep comments short and only where the code cannot say it. No `unsafe` code.
- Regenerate `crates/pi-ai/models.json` from upstream catalog data instead of editing many entries by hand.
- Commit messages use Conventional Commits (`feat(scope): ...`, `fix: ...`, `feat!:` for breaking changes); the release workflow derives versions and CHANGELOG.md from them. Never edit versions or CHANGELOG.md by hand; use `cargo xtask`.
- Never commit unless asked.

# Parity with upstream

- Upstream is `earendil-works/pi` on GitHub. [`docs/parity/upstream.json`](docs/parity/upstream.json) records the upstream commit this port has caught up to.
- When you port an upstream commit, write `upstream <sha>` with the 9-character SHA in the commit message, for example `fix(ai): recover from Copilot request-size overflow (upstream 73f8de80b)`. The parity report marks a commit as ported only on that exact text, so do not write `upstream <sha>` for a commit you did not port.
- Record accepted differences in [`docs/parity/allowlist.json`](docs/parity/allowlist.json) with a reason, and open gaps in [`docs/parity/GAPS.md`](docs/parity/GAPS.md). User-facing differences also go in the README section "Differences from upstream pi".
- `cargo xtask parity --upstream <checkout>` writes the gap report. The Parity workflow runs it daily and updates the issue labeled `parity-report`.

# Playbooks

| Task | Read |
| --- | --- |
| Find gaps, port upstream commits | [docs/parity/README.md](docs/parity/README.md) |
| Close a Node API gap in the extension runtime | [docs/agents/node-compat.md](docs/agents/node-compat.md) |
| Diagnose a CI failure or a flaky test | [docs/agents/ci-triage.md](docs/agents/ci-triage.md) |
| Ship a change | [docs/agents/release.md](docs/agents/release.md) |
| Run the scheduled gap review | [docs/agents/daily-gap-check.md](docs/agents/daily-gap-check.md) |
| Change the Cursor, Grok Build, or Google Antigravity provider | [docs/providers/subscription-providers.md](docs/providers/subscription-providers.md) |

All documents are listed in [docs/INDEX.md](docs/INDEX.md).

# Shared state

- Ask before adding a dependency, pushing to `main`, merging, or changing GitHub settings. Opening a PR from a branch is fine when the task asks for one.
- One change per PR. Name the upstream commits or `unsupported.rs` entries it closes.
