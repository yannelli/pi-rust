# pi-rust

A Rust-only port of the [pi](https://github.com/earendil-works/pi) coding agent harness: a streaming multi-provider LLM API, an agent loop with tool calling, and the `pi` coding agent CLI. No Node.js or JavaScript runtime is required.

## Install

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/yannelli/pi-rust/main/install.ps1 | iex
```

The scripts download the latest release for your platform, check it against `SHA256SUMS`, and install `pi` to `~/.local/bin` (Windows: `%LOCALAPPDATA%\Programs\pi`, which the script adds to your user `Path`). They do not use sudo or send telemetry, and `pi update --self` updates an install made this way. Settings:

- `PI_VERSION`: install this release (for example `0.5.3`) instead of the latest. Example: `curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | PI_VERSION=0.5.3 sh`. In PowerShell, set `$env:PI_VERSION = "0.5.3"` before `irm`.
- `PI_INSTALL_DIR`: install to this directory instead. Do not use `~/.cargo/bin`, because `pi update --self` treats a binary there as a cargo install.
- `PI_RELEASES_URL`: download from a mirror of `https://github.com/yannelli/pi-rust/releases`. The mirror must serve `latest/download/SHA256SUMS` and `download/<tag>/<file>`.

### Manual install

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

Each release file also has a signed build provenance attestation. With the GitHub CLI, `gh attestation verify pi-$tag-$target.tar.gz --repo yannelli/pi-rust` shows that the file came from this repository's Release workflow.

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

`/diff [--stat] [path]` shows the uncommitted git changes in the working directory (pi-rust extension): the `git diff HEAD` stat, the untracked files, and the diff of staged and unstaged changes. `--stat` leaves out the diff, and a path limits the output to that path. Output longer than 500 lines is cut, with a note that gives the number of lines left out.

`/context` shows how much of the current model's context window the conversation uses (pi-rust extension): the model, the window size, the tokens in use with their percent, and where auto-compaction starts. The tokens in use come from the last response's reported usage, plus estimates for newer messages; before a response reports usage, they are an estimate of the next request. A table then gives the estimated tokens of the system prompt, tool definitions, context files, skills, user messages, assistant messages, and tool results, and the free space.

`/export [file]` writes the session to HTML, or to JSONL when the file ends in `.jsonl`, and `pi --export <session.jsonl> [output]` exports a saved session file to HTML. With both, an output path that ends in `.md` writes Markdown (pi-rust extension). The Markdown file has the current branch: a title, the date, models, working directory, tokens, and cost, then each user and assistant turn. Thinking is in collapsed `<details>` blocks, tool calls show their JSON arguments, tool results stop after 200 lines, and images become placeholders.

`/init [instructions]` (pi-rust extension) asks the agent to look through the project in the working directory and write an `AGENTS.md` there with an overview, build, test, and lint commands, code layout, conventions, and gotchas, based only on what it finds. If the directory already has a context file that pi loads (`AGENTS.md`, `CLAUDE.md`, or `AGENTS.override.md`), the agent improves that file in place. Text after `/init` is added as extra instructions. The prompt goes out as a normal message: it shows in the chat, uses the current model, and is saved in the session. As with other built-in commands, `/init` takes precedence over a prompt template or extension command named `init`. It is available in interactive mode only.

`/review [instructions]` (pi-rust extension) asks the agent to review the uncommitted changes in the working directory. The prompt tells it to run `git status` and `git diff HEAD`, read the untracked files, and report bugs, risky changes, and missing tests, ordered by severity with `path:line` references, without editing files. Text after `/review` is added as extra instructions, for example a focus area or a base branch to compare against. Outside a git repository, `/review` shows an error and sends nothing. As with `/init`, the prompt goes out as a normal message: it shows in the chat, uses the current model, and is saved in the session.

`/undo` (pi-rust extension) restores the files that the edit and write tools changed during the last prompt: it writes back their earlier content and deletes the files the agent created. Run it again to go one more prompt back. A file that changed after the agent wrote it, for example by you or a bash command, is skipped and listed. Limits: only edit and write changes are tracked, not bash commands; checkpoints stay in memory for the last 20 prompts of the current session, files over 4 MB are not kept, and everything is lost on exit or when you switch sessions. The conversation does not change; use `/tree` to move in it.

`/retry` (pi-rust extension) runs the last prompt of the current branch again. It moves to the point before that prompt, as `/tree` does, and sends the same text and images as a new prompt. The new answer starts a new branch, and the old answer stays in the tree, where `/tree` can return to it. Custom messages and prompts that extensions sent are skipped, so `/retry` picks the last prompt you sent; pi knows which prompts came from extensions only while the session that received them is open, not after a resume. While the agent works or compacts, or before the first prompt, `/retry` shows an error and sends nothing.

### Prompt history (pi-rust extension)

The interactive editor keeps your prompts across sessions and projects in `~/.pi/agent/prompt-history.jsonl`, one JSON string per line, readable only by you on Unix. Up and Down go through them, newest first. Ctrl+R starts a reverse search: type to filter (case does not matter), press Ctrl+R again for the next older match, Enter to put the match in the editor without sending it, and Esc to go back to your draft. A prompt that starts with a space is not saved to the file.

Set `"promptHistory": {"enabled": false}` in `settings.json` to turn this off, or `"maxEntries"` to keep a different number of prompts (default 1000). To use a different key for the search, set `tui.editor.historySearch` in `keybindings.json`.

## Shell completion and `pi doctor` (pi-rust extension)

`pi completion bash|zsh|fish|powershell` prints a completion script for options, subcommands, and file paths. For `--model` and `--provider` it asks pi for the models of the built-in catalog and `models.json`. `pi completion --help` shows how to install each script, for example:

```sh
echo 'eval "$(pi completion bash)"' >> ~/.bashrc
pi completion fish > ~/.config/fish/completions/pi.fish
```

`pi doctor` prints a short report: the version and target, the install method and path, other `pi` programs earlier on `PATH` (such as the npm pi), errors in `settings.json`, `models.json`, `auth.json`, and `mcp.json`, the providers that have credentials (names and sources, never the secrets), MCP servers whose commands are missing, and what the terminal supports. `pi doctor --json` prints the same report as JSON. It makes no network calls, starts no MCP servers, and exits 1 when it finds a problem.

## Sessions and usage stats (pi-rust extension)

`pi sessions` lists the saved sessions of the current project, newest first: a short id, the last activity, the message count, the cost, and the name or first prompt. `--all` lists every project, `--limit <n>` shows the newest n, and `--json` prints an array.

`pi sessions rm <id-prefix|path>...` deletes sessions by a listed id prefix or a file path; it refuses a prefix that matches more than one session and lists the matches. `pi sessions prune --older-than 30d` deletes sessions with no activity for that long (units `s`, `m`, `h`, `d`, `w`) and prints the bytes it frees; `--dry-run` shows what it would delete. Both ask before they delete on a terminal and need `--yes` otherwise.

`pi stats` totals the sessions of the current project (`--all` for every project): sessions, prompts, assistant turns, input, output, and cache tokens, and cost, by model, by provider, and by day for the last 14 days. `--since 7d` counts only recent activity, and `--json` prints the report as JSON. `pi stats` and `pi sessions list` only read session files, and no session command uses the network. All of them honor `--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, and the `sessionDir` setting.

```sh
pi sessions --all --limit 10
pi sessions prune --older-than 30d --dry-run
pi stats --since 7d
```

## Run limits (pi-rust extension)

`--max-turns <n>` stops a run after n assistant responses to one prompt. Tool results do not count, and each prompt counts from zero. `--max-cost <usd>` stops the run once the session's total cost, as the footer shows it, reaches the limit; after that, a new prompt does not start a run. Both limits apply in every mode, for the current process only.

A limit stops the run like Esc does, after the tool results of the last turn, so the session file stays valid and `pi -c` resumes it. A run that ends by itself on its last allowed turn is not stopped.

```sh
pi -p --max-turns 20 --max-cost 0.50 "Fix the failing tests"
echo $?   # 3 when a limit stopped the run
```

In print mode, pi writes a line such as `pi: stopped after 20 turns (--max-turns)` to stderr, skips the remaining messages, and exits with code 3; `--mode json` does the same. JSON and RPC mode also send a `run_limit` event before `agent_end`:

```json
{"type":"run_limit","limit":"maxTurns","max":20,"turns":20,"cost":0.1234,"message":"stopped after 20 turns (--max-turns)"}
```

`limit` is `maxTurns` or `maxCost`, `max` is the limit, `turns` counts the responses of the run, and `cost` is the session total in US dollars. When the cost limit keeps a prompt from starting a run, the event comes alone and an RPC `prompt` gets the disposition `handled`. Interactive mode shows the message as a status line. An extension that registers its own `--max-turns` or `--max-cost` flag gets the value instead, as with upstream pi.

## Structured output (pi-rust extension)

`--output-schema <file>` makes the final answer of print mode a JSON value that matches a JSON Schema, for scripts. pi adds the schema to the system prompt and checks the last response with the same validator as tool arguments. A single surrounding code fence such as ```` ```json ```` is accepted. When the response is not valid JSON or does not match, pi sends the errors back in the same session and asks for corrected JSON. `--output-retries <n>` sets how many corrective prompts it sends (default 1, `0` for none).

```sh
pi -p --output-schema review.schema.json "Review the diff in HEAD" > review.json
echo $?   # 4 when the answer still does not match
```

On success, text mode prints only the JSON value, compact on one line. `--mode json` ends with an `output` event instead:

```json
{"type":"output","value":{"verdict":"approve","issues":[]}}
```

When the answer still does not match after the retries, pi writes the errors to stderr and exits with code 4; text mode prints nothing to stdout. An error or aborted response exits 1, and a run limit exits 3 as above. pi checks the schema file before it starts: it must be valid JSON, use local `$ref`s only, and use keywords and patterns the validator supports. The flag works only in print mode (`-p`, `--mode json`, or when stdin or stdout is not a terminal); interactive, RPC, and ACP mode reject it. An extension that registers its own `--output-schema` or `--output-retries` flag gets the value instead.

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
| `~/.pi/agent/prompt-history.jsonl` | Submitted prompts for Up, Down, and Ctrl+R (pi-rust only) |

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

With an API key, pi reads `GET <base URL>/models`. This list also names embedding, audio, image, moderation, and realtime models, so pi adds only listed models of a model family that the catalog has, such as `gpt-` or `grok-`, and never models of those kinds. The list has no limits, so the catalog's limits stay. With a ChatGPT login for `openai`, the same request returns the account's models by `slug`, and pi takes each model's `context_window` as its context size. pi does not use the larger `max_context_window`.

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
- Markdown export (see [Use](#use)) is an addition: `/export notes.md` and `pi --export session.jsonl notes.md` write Markdown. Upstream pi exports only HTML and JSONL.
- `/diff` (see [Use](#use)) is an addition; upstream pi has no command that shows the git changes in the working directory.
- `/init` (see [Use](#use)) is an addition; upstream pi has no `/init` command.
- `/review` (see [Use](#use)) is an addition; upstream pi has no `/review` command.
- `/context` (see [Use](#use)) is an addition; upstream pi shows context use only in the footer.
- `/undo` (see [Use](#use)) is an addition; upstream pi has no command that restores the files the agent changed.
- `/retry` (see [Use](#use)) is an addition; upstream pi has no `/retry` command.

- `pi completion` and `pi doctor` (above) are additions; the TypeScript pi has neither.
- `pi sessions` and `pi stats` (above) are additions; the TypeScript pi lists and deletes sessions only in its session picker (`pi -r`, `/resume`) and shows usage only for the current session (`/session`).
- The run limits `--max-turns` and `--max-cost` (above) are additions.
- Structured output with `--output-schema` and `--output-retries` (above) is an addition.
- Account model lists for OpenAI Codex, Anthropic, GitHub Copilot, OpenAI, xAI, and Google (see [Providers](#account-model-lists-pi-rust-extension)) are an addition; upstream pi narrows only OpenRouter.
- [Prompt history](#prompt-history-pi-rust-extension) across sessions, with the Ctrl+R search and the `promptHistory` settings, is an addition; upstream pi keeps prompt history only for the running process.
- `pi-env` looks for remote daemons in a `pi-env-daemons` folder next to the running program. Each release has a `pi-env-daemons-<tag>.tar.gz` with the daemons for Linux, Android (Termux), macOS, and Windows on x86-64 and arm64; extract it next to `pi`.
- Notifications: in interactive mode, a run that takes 10 seconds or more ends with a terminal notification titled "pi" that says "Done" or names the error. pi sends OSC 777 in Ghostty, WezTerm, foot, and rxvt, OSC 99 in kitty, OSC 9 in iTerm2 and Windows Terminal, and BEL in tmux, screen, and other terminals. Fullscreen mode skips the notification while the terminal reports focus. Set `terminal.notifyOnFinish` to `"auto"` (the default), `"osc"`, `"bell"`, or `"off"`, and `terminal.notifyMinSeconds` to change the threshold. Upstream pi has this only as the example extension `notify.ts`.
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
