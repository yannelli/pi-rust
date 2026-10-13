# pi-rust

A Rust-only port of the [pi](https://github.com/earendil-works/pi) coding agent harness: a streaming multi-provider LLM API, an agent loop with tool calling, and the `pi` coding agent CLI. No Node.js or JavaScript runtime is required.

## Install

Download the archive for your platform from [Releases](https://github.com/yannelli/pi-rust/releases), extract it, and put `pi` (or `pi.exe`) on your `PATH`. Each release has `SHA256SUMS`.

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `pi-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `pi-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` |
| Linux armv7 (32-bit) | `pi-vX.Y.Z-armv7-unknown-linux-musleabihf.tar.gz` |
| macOS Apple silicon | `pi-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `pi-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `pi-vX.Y.Z-x86_64-pc-windows-msvc.zip` |
| Windows arm64 | `pi-vX.Y.Z-aarch64-pc-windows-msvc.zip` |

Linux binaries are static and run on any distribution, including Termux. With the GitHub CLI: `gh release download --repo yannelli/pi-rust --pattern '*aarch64-apple-darwin*'`.

To build from source (Rust 1.85 or newer):

```sh
cargo install --path crates/pi-coding-agent
```

## Use

```sh
export ANTHROPIC_API_KEY=...        # or OPENAI_API_KEY, OPENROUTER_API_KEY, ...
pi                                  # interactive session in the current directory
pi -p "Summarize src/main.rs"       # print mode: run once, print the answer
pi --mode json -p "List the TODOs"  # one JSON agent event per line
pi -c                               # continue the latest session here
pi -r                               # pick a session to resume
pi --model openai/gpt-5.5 --thinking high
pi @notes.md @screenshot.png "What changed?"
cat error.log | pi -p "Explain this error"
```

Run `pi --help` for all options. In interactive mode, type `/help` for commands: `/model`, `/thinking`, `/compact`, `/new`, `/session`, `/resume`, `/follow`, `/abort`, `/quit`. Input typed while the agent works is queued as steering. Ctrl+C aborts a response; press it twice to exit.

## ACP (Zed and other editors)

`pi --mode acp` runs pi as an [Agent Client Protocol](https://agentclientprotocol.com) server over stdio, so editors such as Zed can use it as an external agent. In Zed, add this to `settings.json`:

```json
"agent_servers": {
  "pi": {
    "type": "custom",
    "command": "pi",
    "args": ["--mode", "acp"],
    "env": {}
  }
}
```

Each editor session runs a pi session in its project directory, saved in `~/.pi/agent/sessions/` like any other, so the editor can list, resume, and delete them and `pi -r` can resume them too. The editor's model and thinking selectors, slash commands (prompt templates, `/skill:name`, and `/compact`, `/autocompact`, `/export`, `/session`, `/name`, `/steering`, `/follow-up`, `/changelog`), tool call diffs and terminals, and context usage all work. Set `quietStartup: true` in pi's settings to hide the startup info block, and `PI_ACP_ENABLE_EMBEDDED_CONTEXT=true` to accept embedded file contents in prompts. Without configured models, the editor offers to launch pi in a terminal to log in. ACP mode is a port of [pi-acp](https://github.com/svkozak/pi-acp) by Sergii Kozak (MIT).

## Configuration

pi-rust reads the same files as the TypeScript pi, so both can share one setup.

| Path | Purpose |
| --- | --- |
| `~/.pi/agent/models.json` | Custom providers and models; same format as pi (`providers.<name>.baseUrl`, `api`, `apiKey`, `models`, `modelOverrides`) |
| `~/.pi/agent/settings.json` | `defaultProvider`, `defaultModel`, `defaultThinkingLevel`; `<cwd>/.pi/settings.json` overrides them |
| `~/.pi/agent/AGENTS.md` | Global instructions; `AGENTS.md` or `CLAUDE.md` files from the filesystem root down to the working directory are added too |
| `~/.pi/agent/sessions/` | JSONL sessions in pi's v3 format |

`PI_CODING_AGENT_DIR` replaces `~/.pi/agent`. API keys come from `--api-key`, then `models.json` (`$VAR`, `${VAR}`, `!command`, or a literal), then provider environment variables such as `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`.

## Providers

The bundled catalog has about 1,100 models from pi's catalog that use one of three wire protocols:

- `anthropic-messages`: Anthropic, including prompt caching, thinking with signatures, and OAuth tokens.
- `openai-responses`: OpenAI, including reasoning items and encrypted reasoning replay.
- `openai-completions`: OpenAI-compatible endpoints (OpenRouter, Groq, Cerebras, DeepSeek, xAI, Fireworks, Together, Ollama, LM Studio, vLLM and others), with pi's per-provider compatibility settings.

## Crates

| Crate | Contents |
| --- | --- |
| [`pi-ai`](crates/pi-ai) | Message types (JSON-compatible with pi), streaming event protocol, SSE parsing, providers, model registry, context overflow detection, and a scripted `faux` provider for tests |
| [`pi-agent`](crates/pi-agent) | The agent loop, tool argument validation, and the stateful `Agent`. The loop runs tool calls in parallel or in order, accepts steering and follow-up input between turns, and supports tool hooks and cancellation |
| [`pi-coding-agent`](crates/pi-coding-agent) | Tools (`read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`), system prompt, sessions, compaction, and the `pi` CLI |

`grep` and `find` are implemented in Rust with the `ignore` crate; they do not need ripgrep or fd.

## Differences from the TypeScript pi

Ported: the agent loop semantics, the coding tools and their truncation limits, the system prompt structure, project context files, session files with compaction, automatic compaction near the context window and after overflow errors, model selection, and print and JSON modes.

Not ported: the full-screen TUI (pi-rust has a line-based interactive mode), extensions, skills, prompt templates, themes, MCP, `/login` OAuth flows and `auth.json`, session branching and tree navigation, RPC mode, HTML export, image resizing, and the Google, Bedrock, and Mistral wire protocols.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests use the `faux` provider and local mock HTTP servers; they need no API keys or network access.

## Releases

Versions follow [Semantic Versioning](https://semver.org/) and are derived from [Conventional Commits](https://www.conventionalcommits.org/):

| Commit | Release |
| --- | --- |
| `fix:`, `perf:`, `revert:` | patch |
| `feat:` | minor (patch before 1.0) |
| `feat!:`, `fix!:`, or a `BREAKING CHANGE:` footer | major (minor before 1.0) |
| `docs:`, `chore:`, `ci:`, `test:`, `refactor:`, `style:`, `build:` | none |

After CI passes on `main`, the Release workflow runs `cargo xtask plan`. When a release is due it bumps the workspace version, prepends the `CHANGELOG.md` section, commits `chore(release): vX.Y.Z`, tags `vX.Y.Z`, builds every target, and publishes the GitHub release with checksums. Pushing a `vX.Y.Z` tag that matches `Cargo.toml` releases that version directly, and `vX.Y.Z-rc.1` tags publish pre-releases. Run the workflow manually with `dry_run` to build all targets without publishing.

```sh
cargo xtask plan                 # next version and release decision
cargo xtask prepare 0.2.0        # set the version and update CHANGELOG.md
cargo xtask notes 0.2.0          # print release notes
```

## License

MIT. Based on [pi](https://github.com/earendil-works/pi) by Mario Zechner; see [LICENSE](LICENSE).
