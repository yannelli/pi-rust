# pi-rust

A Rust-only port of the [pi](https://github.com/earendil-works/pi) coding agent harness: a streaming multi-provider LLM API, an agent loop with tool calling, and the `pi` coding agent CLI. No Node.js or JavaScript runtime is required.

## Install

Each [release](https://github.com/yannelli/pi-rust/releases) has an archive per platform and a `SHA256SUMS` file. Every archive holds a `pi-vX.Y.Z-<target>/` folder with `pi` (or `pi.exe`), `README.md`, `LICENSE`, and `CHANGELOG.md`.

| Platform | Target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |
| Linux armv7 (32-bit) | `armv7-unknown-linux-musleabihf` |
| macOS Apple silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |
| Windows arm64 | `aarch64-pc-windows-msvc` |

Linux and macOS (Linux binaries are static and run on any distribution, including Termux):

```sh
target=x86_64-unknown-linux-musl   # pick yours from the table
tag=$(curl -fsSL https://api.github.com/repos/yannelli/pi-rust/releases/latest | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
curl -fsSLO "https://github.com/yannelli/pi-rust/releases/download/$tag/pi-$tag-$target.tar.gz"
curl -fsSLO "https://github.com/yannelli/pi-rust/releases/download/$tag/SHA256SUMS"
grep " pi-$tag-$target.tar.gz$" SHA256SUMS | sha256sum -c -   # macOS: shasum -a 256 -c -
tar -xzf "pi-$tag-$target.tar.gz"
mkdir -p ~/.local/bin && install -m 755 "pi-$tag-$target/pi" ~/.local/bin/pi   # ~/.local/bin must be on PATH
pi --version
```

Windows (PowerShell):

```powershell
$target = "x86_64-pc-windows-msvc"   # or aarch64-pc-windows-msvc
$tag = (Invoke-RestMethod https://api.github.com/repos/yannelli/pi-rust/releases/latest).tag_name
Invoke-WebRequest "https://github.com/yannelli/pi-rust/releases/download/$tag/pi-$tag-$target.zip" -OutFile pi.zip
Expand-Archive pi.zip -DestinationPath "$env:LOCALAPPDATA\pi" -Force
$bin = "$env:LOCALAPPDATA\pi\pi-$tag-$target"
[Environment]::SetEnvironmentVariable("Path", "$([Environment]::GetEnvironmentVariable('Path', 'User'));$bin", "User")
```

Open a new terminal afterwards so the `Path` change applies. With the GitHub CLI, `gh release download --repo yannelli/pi-rust --pattern '*<target>*'` fetches the latest archive. A browser download on macOS may need `xattr -d com.apple.quarantine pi` before the first run.

`pi update --self` replaces a release install with the latest release, and `pi update --models` refreshes the model catalogs. To build from source (Rust 1.85 or newer): `cargo install --path crates/pi-coding-agent`.

## Use

```sh
export ANTHROPIC_API_KEY=...        # or OPENAI_API_KEY, GEMINI_API_KEY, ...; or run /login
pi                                  # interactive session in the current directory
pi -p "Summarize src/main.rs"       # print mode: run once, print the answer
pi --mode json -p "List the TODOs"  # one JSON event per line
pi --mode rpc                       # JSON-RPC over stdio for other programs
pi -c                               # continue the latest session here
pi -r                               # pick a session to resume
pi --model openai/gpt-5.5 --thinking high
pi @notes.md @screenshot.png "What changed?"
cat error.log | pi -p "Explain this error"
pi install npm:<package>            # install a pi package (extensions, skills, themes)
```

Run `pi --help` for all options. In interactive mode, type `/` to list commands, for example `/login`, `/model`, `/settings`, `/tree`, `/fork`, `/compact`, `/export`, `/share`, `/mcp`, and `/hotkeys`. Input typed while the agent works is queued as steering, and Alt+Enter (Ctrl+Q on Windows) queues a follow-up.

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

pi-rust reads the same files as the TypeScript pi, so both can share one setup. `PI_CODING_AGENT_DIR` replaces `~/.pi/agent`.

| Path | Purpose |
| --- | --- |
| `~/.pi/agent/auth.json` | API keys and OAuth logins from `/login` or `pi auth` |
| `~/.pi/agent/settings.json` | Settings; `<project>/.pi/settings.json` overrides them for a trusted project |
| `~/.pi/agent/models.json` | Custom providers and models |
| `~/.pi/agent/keybindings.json` | Key bindings |
| `~/.pi/agent/mcp.json`, `<project>/.pi/mcp.json` | MCP servers |
| `~/.pi/agent/{extensions,skills,prompts,themes}/` | Extensions, skills, prompt templates, and themes; `.pi/` in a project and `~/.agents/skills` work too |
| `AGENTS.md`, `CLAUDE.md`, `SYSTEM.md`, `APPEND_SYSTEM.md` | Context files and system prompt overrides |
| `~/.pi/agent/sessions/` | Sessions as JSONL trees in pi's format |

On first start, pi-rust moves files from older pi layouts the same way pi does (for example `oauth.json` into `auth.json`).

## Providers

pi-rust has every wire protocol of pi: `anthropic-messages`, `openai-responses`, `openai-completions`, `openai-codex-responses` (SSE and WebSocket), `azure-openai-responses`, `google-generative-ai`, `google-vertex`, `bedrock-converse-stream`, `mistral-conversations`, and `pi-messages`. The bundled catalog is pi's, and `pi update --models` refreshes it. `/login` supports OAuth for Anthropic, OpenAI Codex, GitHub Copilot, OpenRouter, xAI, Kimi, Meta, and Radius, plus API keys and ambient AWS and Google credentials. llama.cpp servers and virtual models work as in pi.

### OpenRouter

Authenticate with `/login openrouter` or `OPENROUTER_API_KEY`.

With a key, pi lists only the OpenRouter chat models that key may use, with the key's context size, max output, and prices. These come from OpenRouter's `/models/user` endpoint and reflect guardrails and provider restrictions set in OpenRouter. Image and classifier models are not filtered.

pi refreshes the list in the background at startup in interactive, RPC, and ACP modes, when `/model` opens, after `/login`, and on `pi update --models`. The last list is saved in `models-store.json` and used offline, in `pi -p`, and until a refresh finishes. After you switch keys outside `/login`, the list of the old key stays until the next refresh.

Regional endpoints filter models to their region. To use one, set the base URL in `models.json`:

```json
{
  "providers": {
    "openrouter": { "baseUrl": "https://us.openrouter.ai/api/v1" }
  }
}
```

If the base URL rejects the key at `/models/user` or does not have that endpoint, for example a proxy configured as the OpenRouter base URL, pi lists every OpenRouter model.

### Account model lists (pi-rust extension)

pi-rust narrows more providers to the models your account or key may use, like OpenRouter above. Upstream pi does not. With a credential, pi requests the provider's model list at most once every 4 hours, and at once after `/login` and on `pi update --models`. It drops catalog chat models that the list does not name, takes context size and max output from the list when it has them, and adds listed chat models that the catalog lacks as copies of the closest catalog model. Image and classifier models are not filtered. If the list does not name the provider's default model, pi drops nothing and only takes limits and adds models. The list is saved in `models-store.json` with the pi version and without the credential, and the base URL set in `models.json` applies. After an upgrade, pi shows the full catalog until a new list arrives. Model list requests do not follow redirects, so the credential goes only to the configured host.

To get the full catalog, point the provider's `baseUrl` in `models.json` at an endpoint without the model list, such as a proxy. pi lists every catalog model when the model list answers 401, 403, or 404, or when its body cannot be read. A list that names no catalog chat model, or an empty list, shows the full catalog and is not saved. Other failures, redirects included, keep the last saved list.

#### OpenAI Codex

With a ChatGPT login, pi reads the account's models from the Codex backend (`<base URL>/codex/models`), the list that Codex shows, and skips hidden models. This endpoint is not a public API, so pi reads it leniently.

#### Anthropic

With an API key or a Claude subscription login, pi reads `GET /v1/models` with the provider's authentication headers and follows its pages. The list gives each model's context size and max output.

#### GitHub Copilot

With a Copilot login, pi reads `GET /models` from the login's Copilot endpoint and keeps the chat models that support tools, that the model picker shows, and that a policy does not disable, as `/login` chooses them. The token's endpoint comes before the `models.json` base URL, as for requests.

#### OpenAI and xAI

With an API key, pi reads `GET <base URL>/models`. This list also names embedding, audio, image, moderation, and realtime models, so pi adds only listed models of a model family that the catalog has, such as `gpt-` or `grok-`, and never models of those kinds. The list has no limits, so the catalog's limits stay.

#### Google

With a Gemini API key, pi reads `GET <base URL>/models` with `x-goog-api-key`, follows its pages, and uses only the models that support `generateContent`, with their input and output token limits. New models are added only for a model family that the catalog has.

#### Not covered

Cursor and Grok Build have no public model list API for other harnesses, so pi-rust does not narrow them. Other providers show the full catalog.

## Crates

| Crate | Contents |
| --- | --- |
| [`pi-ai`](crates/pi-ai) | Message types (JSON-compatible with pi), streaming events, providers, OAuth, the model registry and catalogs |
| [`pi-agent`](crates/pi-agent) | The agent loop, tool argument validation, and the stateful `Agent` |
| [`pi-tui`](crates/pi-tui) | The terminal UI library: differential rendering, editor, markdown, images, and components |
| [`pi-mcp`](crates/pi-mcp) | The MCP client (stdio and HTTP, OAuth) and server manager |
| [`pi-codemode`](crates/pi-codemode) | Codemode: a sandboxed JavaScript runtime the model scripts tools in |
| [`pi-js`](crates/pi-js) | Embedded QuickJS with Node-style modules and TypeScript, for pi extensions |
| [`pi-coding-agent`](crates/pi-coding-agent) | Tools, sessions, compaction, resources, extensions, the interactive TUI, print, JSON, RPC, and ACP modes, and the `pi` CLI |
| [`pi-chord`](crates/pi-chord) | Chord: an application composition runtime for services, replicated state, RPC, and plugins |
| [`pi-telemetry`](crates/pi-telemetry) | Vendor-neutral telemetry contracts and typed schema utilities |
| [`pi-schema`](crates/pi-schema) | TypeBox-compatible JSON Schema checks, defaults, and cleaning, shared by pi-ai and pi-chord |
| [`pi-embedded`](crates/pi-embedded) | In-process pi sessions for native and Rust applications, with a C ABI (`include/pi_embedded.h`) |
| [`pi-protocol`](crates/pi-protocol), [`pi-client`](crates/pi-client), [`pi-server`](crates/pi-server) | The CBOR wire protocol, its client, and its server over Unix sockets |
| [`pi-durable`](crates/pi-durable) | Pi Durable: durable sessions on memory, SQLite, or JSONL storage, the harness, and its coding tools |
| [`pi-env`](crates/pi-env), [`pi-env-daemon`](crates/pi-env-daemon) | Remote execution environments for Pi Durable over SSH, and the `pi-env` daemon that runs on the remote machine |
| [`pi-evals`](crates/pi-evals) | Behavioral evals for the coding agent: host evals and a documentation-lift comparison in Docker (not published) |

## Differences from the TypeScript pi

pi-rust ports the `pi` CLI and its libraries feature for feature. These parts differ:

- Extensions run on embedded QuickJS, so no Node install is needed. Every Node built-in module loads, including `zlib` (gzip, brotli, and zstd), `async_hooks` with `AsyncLocalStorage`, `v8` serialization, and the networking modules `net`, `tls`, `dgram`, `dns`, `http`, `https`, and `http2`, `vm` contexts with timeouts, `worker_threads`, `cluster` (with socket and server handle passing over IPC), `node:sqlite` on the bundled SQLite, `node:test` with TAP and spec output, `repl` over any streams, in-process `inspector` sessions, and `wasi` with a `WebAssembly` global on the wasmi interpreter; the APIs that do not work (for example `crypto.subtle` beyond `digest`, and rare `crypto` ciphers such as Camellia) throw `ERR_PI_UNSUPPORTED`, and `crates/pi-js/src/unsupported.rs` lists them all. Native npm addons cannot load.
- `grep` and `find` use the Rust `ignore` crate, so pi-rust does not download `rg` or `fd`.
- The version check and `pi update --self` use this repository's releases.
- CJK and Thai word movement uses ICU4X dictionaries, so a few word boundaries differ from Node's `Intl.Segmenter`.
- `pi --mode acp` (above) is an addition; the TypeScript pi needs the separate pi-acp adapter.
- Account model lists for OpenAI Codex, Anthropic, GitHub Copilot, OpenAI, xAI, and Google (see [Providers](#account-model-lists-pi-rust-extension)) are an addition; upstream pi narrows only OpenRouter.
- `pi-env` looks for remote daemons in a `pi-env-daemons` folder next to the running program. Each release has a `pi-env-daemons-<tag>.tar.gz` with the daemons for Linux, Android (Termux), macOS, and Windows on x86-64 and arm64; extract it next to `pi`.
- `pi experimental` (`server`, `client`, `durable`, `vacation`) needs `PI_EXPERIMENTAL=1`, as upstream. Its `server` and `client` use Unix sockets, so on Windows they report that they need Unix; upstream has no Windows transport either.

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
