<p align="center">
  <a href="https://github.com/yannelli/pi-rust">
    <img alt="pi-rust logo" src="docs/assets/pi-rust-logo.svg" width="128">
  </a>
</p>

# pi-rust

pi-rust is a Rust port of the [pi](https://github.com/earendil-works/pi) coding agent harness. It has these parts:

- A streaming API for large language models (LLMs) from many providers.
- An agent loop that calls tools.
- The `pi` coding agent, which has a command-line interface (CLI).

pi-rust does not need Node.js or a different JavaScript runtime. In this file, "upstream pi" is the original pi, which is written in TypeScript.

## Install

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/yannelli/pi-rust/main/install.ps1 | iex
```

The script downloads the latest release, compares it with `SHA256SUMS`, and installs `pi`. For script options, manual install, and build from source, see [Install](crates/pi-coding-agent/docs/install.md).

## Quick start

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

Run `pi --help` to see all options. In interactive mode, type `/` to see the commands.

## Documentation

| Page | Contents |
| --- | --- |
| [Install](crates/pi-coding-agent/docs/install.md) | Script options, manual install, provenance check, update, and build from source |
| [Interactive commands](crates/pi-coding-agent/docs/commands.md) | `/diff`, `/context`, `/export`, `/init`, `/review`, `/undo`, `/retry`, and prompt history |
| [CLI commands and flags](crates/pi-coding-agent/docs/cli.md) | `pi completion`, `pi doctor`, `pi sessions`, `pi stats`, run limits, structured output, and `pi extensions` |
| [Extension marketplace](crates/pi-coding-agent/docs/marketplace.md) | Find, install, update, and remove native Rust extensions |
| [Providers](crates/pi-coding-agent/docs/providers.md) | Wire protocols, logins, Cursor, Google Antigravity, `disabledProviders`, OpenRouter, and account model lists |
| [Status line](crates/pi-coding-agent/docs/status-line.md) | The `marketplace:statusline` extension and the move from the built-in status line |
| [Terminal](crates/pi-coding-agent/docs/terminal.md) | Themes, notifications, and the `marketplace:iterm2` extension |
| [ACP mode](crates/pi-coding-agent/docs/acp.md) | `pi --mode acp` for Zed and other editors |
| [ACP agents](crates/pi-coding-agent/docs/acp-providers.md) | Kiro CLI, Amp, and ACP registry agents as providers, `/acp`, and the `acpAgents` setting |
| [Docs index](docs/INDEX.md) | Agent playbooks, parity, and the protocol notes of the subscription providers |

## Configuration

pi-rust reads the same files as upstream pi, so the two can use one setup. To use a different folder in place of `~/.pi/agent`, set `PI_CODING_AGENT_DIR`.

| Path | Contents |
| --- | --- |
| `~/.pi/agent/auth.json` | API keys and OAuth logins from `/login` or `pi auth` |
| `~/.pi/agent/settings.json` | Settings. In a trusted project, `<project>/.pi/settings.json` overrides them. |
| `~/.pi/agent/models.json` | Custom providers and models |
| `~/.pi/agent/keybindings.json` | Key bindings |
| `~/.pi/agent/mcp.json`, `<project>/.pi/mcp.json` | MCP servers |
| `~/.pi/agent/{extensions,skills,prompts,themes}/` | Extensions, skills, prompt templates, and themes. `.pi/` in a project and `~/.agents/skills` also work. |
| `AGENTS.md`, `CLAUDE.md`, `SYSTEM.md`, `APPEND_SYSTEM.md` | Context files and system prompt overrides |
| `~/.pi/agent/sessions/` | Sessions as JSONL trees in the pi format |
| `~/.pi/agent/prompt-history.jsonl` | The prompts that you sent, for Up, Down, and Ctrl+R (pi-rust only) |

When pi-rust starts for the first time, it moves files from older pi layouts, as upstream pi does. For example, it moves `oauth.json` into `auth.json`.

## Differences from upstream pi

pi-rust has all the features of the upstream `pi` CLI and its libraries. This section gives the differences.

### Runtime

- New Kimi Code OAuth logins use the global `.ai` endpoints. To sign in to mainland China, set `KIMI_CODE_OAUTH_HOST=https://auth.kimi.com`. The login stores its OAuth and API endpoints for later refreshes; older logins without this metadata retain their mainland endpoints. Explicit `KIMI_CODE_OAUTH_HOST` and `KIMI_CODE_BASE_URL` values override saved endpoints. A custom model `baseUrl` in `models.json` stays in effect. Kimi and Moonshot tool requests infer omitted types from enums, constants, and unambiguous schema structure.
- Extensions run on an embedded QuickJS engine, so pi-rust does not need Node.js. All Node built-in modules load. These include:
  - `zlib` with gzip, brotli, and zstd.
  - `async_hooks` with `AsyncLocalStorage`, and `v8` serialization.
  - The network modules `net`, `tls`, `dgram`, `dns`, `http`, `https`, and `http2`.
  - `vm` contexts with timeouts, `worker_threads`, and `cluster`. They can send socket and server handles over inter-process communication (IPC).
  - `node:sqlite` on the bundled SQLite, `node:test` with TAP and spec output, and `repl` over any streams.
  - In-process `inspector` sessions, and `wasi` with a `WebAssembly` global on the wasmi interpreter.

  The APIs that do not work throw `ERR_PI_UNSUPPORTED`. Examples are `crypto.subtle` other than `digest`, and rare `crypto` ciphers such as Camellia. `crates/pi-js/src/unsupported.rs` lists all of them. Native npm addons cannot load on QuickJS.

  To run extensions on Node 22 or later, set `extensionRuntime` to `"node"` or `"auto"`, or use `--extension-runtime` or `PI_EXTENSION_RUNTIME`. On Node, native addons, `child_process.fork`, and V8-only modules work. If Node is missing, pi uses QuickJS. See [Node backend](docs/agents/node-compat.md#node-backend).
- IPC handle retries use a pi-js-specific wrapper. Internal handle listeners receive an object with only `close()`, and cluster connections queued to a disconnected worker are not reclaimed. See [Extension runtime](docs/parity/GAPS.md#extension-runtime-pi-js).
- The providers that extensions get from `@earendil-works/pi-ai/providers/all` stream, classify, generate images, and resolve auth in Rust. Their API key login asks for one key, also for Amazon Bedrock, Google Vertex AI, and Cloudflare. See [Extension module exports](docs/parity/GAPS.md#extension-module-exports).
- When you quit the interactive mode, pi waits at most 3 seconds for the running turn and the `session_shutdown` handlers, then exits. Upstream pi waits until they finish.
- `grep` and `find` use the Rust `ignore` crate, so pi-rust does not download `rg` or `fd`.
- The version check and `pi update --self` use the releases of this repository.
- Word movement in Chinese, Japanese, Korean (CJK), and Thai text uses ICU4X dictionaries. Thus some word boundaries are different from `Intl.Segmenter` in Node.
- `pi-env` looks for remote daemons in a `pi-env-daemons` folder next to the `pi` program. Each release has a `pi-env-daemons-<tag>.tar.gz` archive with the daemons for Linux, Android (Termux), macOS, and Windows on x86-64 and arm64. Extract this archive in the folder that contains `pi`.
- `pi experimental` and its commands `server`, `client`, `durable`, and `vacation` need `PI_EXPERIMENTAL=1`, as in upstream pi. `server` and `client` use Unix sockets. On Windows, they show a message that they need Unix. Upstream pi also has no Windows transport.
- When a local shell command exits, pi-durable still stops waiting for output 100 ms after the exit or the last chunk after it, as upstream does. It then delivers queued chunks and any chunk the readers finish reading before they observe the stop signal. On Unix it also takes at most 64 KiB more per stream if readable without waiting. On Windows it starts no further read. See [Platform](docs/parity/GAPS.md#platform).
- Extension and login `select` menus show at most 10 options and fewer in short terminals, with a `(n/total)` counter, so the selected option stays visible. Upstream pi shows all options.

### Additions

| Addition | Upstream pi | Docs |
| --- | --- | --- |
| The slash commands `/diff`, `/init`, `/review`, `/context`, `/undo`, and `/retry` | Has none of them. Shows context use only in the footer. | [Interactive commands](crates/pi-coding-agent/docs/commands.md) |
| Markdown export: `/export notes.md` and `pi --export session.jsonl notes.md` | Exports only HTML and JSONL | [Export](crates/pi-coding-agent/docs/commands.md#export) |
| Prompt history across sessions, with the Ctrl+R search and the `promptHistory` settings | Keeps prompt history only while the process runs | [Prompt history](crates/pi-coding-agent/docs/commands.md#prompt-history) |
| Esc clears the input draft when the agent is idle, and undo restores the draft. Submitted slash commands join prompts and shell commands in the Up and Down history. | | |
| `pi completion` and `pi doctor` | Has neither command | [CLI](crates/pi-coding-agent/docs/cli.md) |
| `pi sessions` and `pi stats` | Lists and deletes sessions only in its session picker (`pi -r` and `/resume`). Shows usage only for the current session (`/session`). | [Sessions](crates/pi-coding-agent/docs/cli.md#sessions) |
| `pi extensions` lists, installs, removes, enables, and disables extensions through the package and resource settings | | [Extensions](crates/pi-coding-agent/docs/cli.md#extensions) |
| `marketplace:` package sources and `pi extensions search` and `info` use signed indexes and checksummed native binaries | Has no Rust extension marketplace | [Marketplace](crates/pi-coding-agent/docs/marketplace.md) |
| The `/extensions` browser searches, installs, updates, removes, enables, and disables extensions, then offers a reload | Has no `/extensions` command | [/extensions](crates/pi-coding-agent/docs/commands.md#extensions) |
| Run limits (`--max-turns`, `--max-cost`) and structured output (`--output-schema`, `--output-retries`) | | [Run limits](crates/pi-coding-agent/docs/cli.md#run-limits) |
| `pi --mode acp`. Permission modes are separate from thinking levels and control tool execution. | Needs the separate pi-acp adapter | [ACP mode](crates/pi-coding-agent/docs/acp.md) |
| Account model lists for OpenAI Codex, Anthropic, GitHub Copilot, OpenAI, xAI, Google, and Cursor | Limits only OpenRouter | [Account model lists](crates/pi-coding-agent/docs/providers.md#account-model-lists) |
| `/usage`, `pi usage`, and the native extension method `usage.get` show the subscription limits of the Claude, ChatGPT (Codex), Cursor, SuperGrok (`xai`, with Grok Build), and Google Antigravity logins: the 5-hour and weekly windows, the used percentage, and the reset time | Has no usage report | [Subscription usage limits](docs/providers/usage.md) |
| Fast mode for models that have one: `/fast`, `--fast`, the `fastMode` setting, the ACP **Speed** option, and RPC `set_fast_mode` | Has no fast mode | [Fast mode](crates/pi-coding-agent/docs/providers.md#fast-mode-pi-rust-extension) |
| The providers `cursor` and `google-antigravity`. Antigravity supports `/acp mode`, `/model`, and `/thinking` before a prompt, with the agent confirming selections before pi saves them. | Has none of them | [Subscription providers](crates/pi-coding-agent/docs/providers.md#cursor-and-google-antigravity-pi-rust-extension) |
| ACP agents as providers: Kiro CLI (V3), Amp, and the agents of the ACP registry run over stdio with their own tools. `/acp` installs them from the registry in the global or project settings, logs in, enables, disables, moves, and sets the default. The `acpAgents` setting stores them. They get pi's MCP servers, their slash commands complete in the editor, and `/undo` covers their file changes. | Has no ACP client | [ACP agents](crates/pi-coding-agent/docs/acp-providers.md) |
| The global `disabledProviders` setting hides providers from model selection and keeps their credentials. The TUI `/settings` menu has a **Disabled providers** list. | | [Disable providers](crates/pi-coding-agent/docs/providers.md#disable-providers) |
| `/settings` items for the `retry.*` and `retry.provider.*` values | Sets these values only in `settings.json` | [Retry settings](crates/pi-coding-agent/docs/providers.md#retry-settings) |
| A terminal notification when a run takes 10 seconds or more | Has this only as the example extension `notify.ts` | [Notifications](crates/pi-coding-agent/docs/terminal.md#notifications) |
| The status line and `/statusline` are the `marketplace:statusline` extension (`pi install marketplace:statusline`). It reads the legacy `statusLine` setting. Interactive mode offers to install it when `statusLine` is set. | Changes the footer only through extensions | [Status line](crates/pi-coding-agent/docs/status-line.md) |
| More harness themes | | [Themes](crates/pi-coding-agent/docs/terminal.md#themes) |
| The iTerm2 integration and `/terminal-setup` are the `marketplace:iterm2` extension (`pi install marketplace:iterm2`). It reads the legacy `terminal.iterm2` setting. Interactive mode offers to install it when `terminal.iterm2` is on. | | [iTerm2 integration](crates/pi-coding-agent/docs/terminal.md#iterm2-integration) |
| Extension UI calls: `ctx.ui.writeTerminal(data, { tmuxPassthrough })` writes raw bytes to the terminal. Footer data of `ctx.ui.setFooter` also has `getSessionStats()` (`input`, `output`, `cacheRead`, `cacheWrite`, `cost`, `contextTokens`, `contextWindow`, `contextPercent`) and `getRunState()` (`idle`, `working`, `blocked`, `done`, or `error`). | Has none of them | |
| Native extensions: a package lists programs in `package.json` `pi.native`, and pi runs each one as an extension over a JSON-RPC protocol on stdio | Loads only TypeScript and JavaScript extensions | [Native extension protocol](docs/extensions/native-protocol.md) |
| Native extensions write their own top-level settings key in the global or a trusted project `settings.json` with `settings.set`. pi refuses its own setting keys. | Extensions have no settings write | [settings.set](docs/extensions/native-protocol.md#settings) |
| `pi.registerProvider` takes `oauth: { builtin: "<provider id>", name? }`: the provider logs in and refreshes with a login flow of pi, and stores the credential under its own id | Reuses a flow only through an imported OAuth object | [provider.register](docs/extensions/native-protocol.md#providers) |
| Extension widgets above and below the editor share a budget of a third of the terminal rows. Over the budget, each widget shows its first line. Alt+W (`app.widgets.toggle`) and `/widgets collapse`, `expand`, or `auto` change this, and `/widgets` shows all widgets in full in the chat. | Caps only text widgets, at 10 lines each | [/widgets](crates/pi-coding-agent/docs/commands.md#widgets) |
| A choice of extension engine: the `extensionRuntime` setting, `--extension-runtime`, and `PI_EXTENSION_RUNTIME` (`quickjs`, `node`, or `auto`) | Always runs extensions on Node | [Extension runtime](crates/pi-coding-agent/docs/cli.md#extension-runtime) |

### Bug fixes that are open upstream

The numbers are upstream issue numbers.

- If you remove a tool from the first tool list, later Anthropic requests do not fail (upstream issue 11376).
- On Bedrock, `strict: "prefer"` tools change to non-strict when the strict tools have more than 16 optional parameters (11416).
- `modelOverrides` in `models.json` also apply to classifier and image models: `name`, `input`, `inputLimits`, and `cost`. For classifiers, `contextWindow` and `compat` also apply (11223).
- The editor removes escape sequences from pasted text. If the end marker of a paste does not arrive, the paste ends after 10 seconds without input or at 16 MiB. Then Ctrl+C works again (11383).
- MCP HTTP replies that are plain JSON obey `maxMessageBytes` (11335).
- A code mode script can declare its own top-level `tools` or `console` (11226).
- Branch summaries from `/tree` use the selected thinking level (11444). If you resume a session after `/tree`, pi restores the thinking level that you used last (11451).
- pi removes mouse reports and other escape sequences with private parameters, such as `ESC[<35;59;1M`, from tool output (11377).
- In `pi -p`, a prompt that an extension command sends with `sendUserMessage` runs before pi exits (11238).
- An unpinned `-e npm:` package updates when npm has a newer version (11375).
- `pi.exec` keeps a maximum of 16 MiB of output from a command. It stops the command when the output gets to that limit (11360).
- `pi-env` watches a directory again after the kernel drops the watch and the directory keeps its inode (11342).

## Crates

| Crate | Contents |
| --- | --- |
| [`pi-ai`](crates/pi-ai) | Message types (JSON-compatible with pi), streaming events, providers, OAuth, the model registry, and catalogs |
| [`pi-agent`](crates/pi-agent) | The agent loop, tool argument validation, and the stateful `Agent` |
| [`pi-tui`](crates/pi-tui) | The terminal UI library: differential rendering, editor, markdown, images, and components |
| [`pi-mcp`](crates/pi-mcp) | The MCP client (stdio and HTTP, OAuth) and server manager |
| [`pi-codemode`](crates/pi-codemode) | Codemode: a sandboxed JavaScript runtime in which the model writes scripts that call tools |
| [`pi-js`](crates/pi-js) | Embedded QuickJS with Node-style modules and TypeScript, for pi extensions |
| [`pi-coding-agent`](crates/pi-coding-agent) | Tools, sessions, compaction, resources, extensions, the interactive TUI, the print, JSON, RPC, and ACP modes, and the `pi` CLI |
| [`pi-chord`](crates/pi-chord) | Chord: an application composition runtime for services, replicated state, RPC, and plugins |
| [`pi-telemetry`](crates/pi-telemetry) | Vendor-neutral telemetry contracts and typed schema utilities |
| [`pi-schema`](crates/pi-schema) | TypeBox-compatible JSON Schema checks, defaults, and value cleanup, for pi-ai and pi-chord |
| [`pi-embedded`](crates/pi-embedded) | In-process pi sessions for native and Rust applications, with a C ABI (`include/pi_embedded.h`) |
| [`pi-protocol`](crates/pi-protocol), [`pi-client`](crates/pi-client), [`pi-server`](crates/pi-server) | The CBOR wire protocol, its client, and its server over Unix sockets |
| [`pi-durable`](crates/pi-durable) | Pi Durable: durable sessions on memory, SQLite, or JSONL storage, the harness, and its coding tools |
| [`pi-env`](crates/pi-env), [`pi-env-daemon`](crates/pi-env-daemon) | Remote execution environments for Pi Durable over SSH, and the `pi-env` daemon that runs on the remote machine |
| [`pi-evals`](crates/pi-evals) | Behavioral evals for the coding agent: host evals and a documentation-lift comparison in Docker (not published) |

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The tests use the `faux` provider and local mock HTTP servers. They do not need API keys or network access.

## Releases

Versions obey [Semantic Versioning](https://semver.org/). The commit messages set the next version, as in [Conventional Commits](https://www.conventionalcommits.org/):

| Commit | Release |
| --- | --- |
| `fix:`, `perf:`, `revert:` | patch |
| `feat:` | minor (patch before 1.0) |
| `feat!:`, `fix!:`, or a `BREAKING CHANGE:` footer | major (minor before 1.0) |
| `docs:`, `chore:`, `ci:`, `test:`, `refactor:`, `style:`, `build:` | none |

After continuous integration (CI) passes on `main`, the Release workflow runs `cargo xtask plan`. If a release is necessary, the workflow does these steps:

1. It increases the workspace version.
2. It adds the new section at the top of `CHANGELOG.md`.
3. It commits `chore(release): vX.Y.Z` and adds the tag `vX.Y.Z`.
4. It builds all targets.
5. It publishes the GitHub release with checksums.

Other ways to start the workflow:

- Push a `vX.Y.Z` tag that matches `Cargo.toml`. The workflow releases that version.
- Push a `vX.Y.Z-rc.1` tag. The workflow publishes a pre-release.
- Run the workflow manually with `dry_run`. The workflow builds all targets and does not release.
- Run the workflow manually with `bump` set to `patch`, `minor`, or `major`, and with `dry_run` off. The workflow releases when the commits do not need a release.

```sh
cargo xtask plan                 # next version and release decision
cargo xtask prepare 0.2.0        # set the version and update CHANGELOG.md
cargo xtask notes 0.2.0          # print release notes
```

## License

MIT. pi-rust is based on [pi](https://github.com/earendil-works/pi) by Mario Zechner. See [LICENSE](LICENSE).

These parts come from other projects. Each release archive includes their licenses.

| Part | Source | License file |
| --- | --- | --- |
| ACP mode | [pi-acp](https://github.com/svkozak/pi-acp) by Sergii Kozak | [`LICENSE-pi-acp`](crates/pi-coding-agent/src/acp/LICENSE-pi-acp) |
| Some JavaScript modules of the extension runtime | [Node.js](https://github.com/nodejs/node) | [`LICENSE-node`](crates/pi-js/src/js/LICENSE-node) |
| JSON syntax diagnostics in the extension runtime | [V8](https://github.com/nodejs/node/tree/v22.12.0/deps/v8) | [`LICENSE-v8`](crates/pi-js/src/js/LICENSE-v8) |
| MCP OAuth code, ported from upstream pi | [MCP TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk) | [`LICENSE-mcp-typescript-sdk`](crates/pi-mcp/src/oauth/LICENSE-mcp-typescript-sdk) |
| Warning colors of `loadProjectContextFiles` in the extension SDK | [chalk](https://github.com/chalk/chalk) 6.0.0 by Sindre Sorhus | [`LICENSE-chalk`](crates/pi-coding-agent/src/extensions/js/LICENSE-chalk) |
| File locks and exit cleanup for `ProjectTrustStore` | [proper-lockfile](https://github.com/moxystudio/node-proper-lockfile) 4.1.2 and [signal-exit](https://github.com/tapjs/signal-exit) 3.0.7 | [`LICENSE-proper-lockfile`](crates/pi-coding-agent/src/extensions/js/LICENSE-proper-lockfile), [`LICENSE-signal-exit`](crates/pi-coding-agent/src/extensions/js/LICENSE-signal-exit) |

The release archives also include `THIRD-PARTY-LICENSES.md` with the licenses of the third-party Rust crates in the binaries. The Release workflow writes it with [cargo-about](https://github.com/EmbarkStudios/cargo-about) from `about.toml` and `about.hbs`.
