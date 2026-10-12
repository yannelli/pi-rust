# Development Rules

- Rust-only port of the TypeScript pi harness. Keep JSON field names, session files, and config paths compatible with upstream pi.
- After code changes run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`. Fix every warning.
- Tests must not use real provider APIs or keys: use `pi_ai::providers::faux` or a local mock HTTP server.
- Providers emit events only through `pi_ai::MessageBuilder`, and every stream must end with `Done` or `Error`.
- Keep comments short and only where the code cannot say it. No `unsafe` code.
- Regenerate `crates/pi-ai/models.json` from upstream catalog data instead of editing many entries by hand.
- Commit messages use Conventional Commits (`feat(scope): ...`, `fix: ...`, `feat!:` for breaking changes); the release workflow derives versions and CHANGELOG.md from them. Never edit versions or CHANGELOG.md by hand; use `cargo xtask`.
- Never commit unless asked.
