<p align="center">
  <a href="https://github.com/yannelli/pi-rust">
    <img alt="pi-rust logo" src="docs/assets/pi-rust-logo.svg" width="128">
  </a>
</p>

# pi-rust

pi-rust is a port of the [pi](https://github.com/earendil-works/pi) coding agent harness, written only in Rust. It has these parts:

- A streaming API for large language models (LLMs) from many providers.
- An agent loop that calls tools.
- The `pi` coding agent, which has a command-line interface (CLI).

You do not need Node.js or any other JavaScript runtime installed. In this file, "upstream pi" is the original pi, which is written in TypeScript.

## Install

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/yannelli/pi-rust/main/install.ps1 | iex
```

The scripts download the latest release for your platform and compare it with `SHA256SUMS`. Then they install `pi` in `~/.local/bin`. On Windows, they install `pi` in `%LOCALAPPDATA%\Programs\pi` and add that folder to your user `Path`. The scripts do not use sudo and do not send telemetry. `pi update --self` can update an installation from these scripts.

To change the installation, set these environment variables:

- `PI_VERSION`: install this release, for example `0.5.3`, and not the latest release. Example: `curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | PI_VERSION=0.5.3 sh`. In PowerShell, set `$env:PI_VERSION = "0.5.3"` before `irm`.
- `PI_INSTALL_DIR`: install `pi` in this directory. Do not use `~/.cargo/bin`, because `pi update --self` treats a binary there as a cargo installation.
- `PI_RELEASES_URL`: download from a mirror of `https://github.com/yannelli/pi-rust/releases`. The mirror must serve `latest/download/SHA256SUMS` and `download/<tag>/<file>`.

### Manual install

Each [release](https://github.com/yannelli/pi-rust/releases) has one archive for each platform and a `SHA256SUMS` file. Each archive contains a `pi-vX.Y.Z-<target>/` folder. The folder contains `pi` or `pi.exe`, `README.md`, `LICENSE`, `LICENSE-pi-acp`, `LICENSE-node`, `LICENSE-mcp-typescript-sdk`, `THIRD-PARTY-LICENSES.md`, and `CHANGELOG.md`.

| Platform | Target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |
| Linux armv7 (32-bit) | `armv7-unknown-linux-musleabihf` |
| macOS Apple silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |
| Windows arm64 | `aarch64-pc-windows-msvc` |

Linux and macOS. The Linux binaries are static, so they run on all distributions, including Termux:

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

Then open a new terminal. The `Path` change applies only to new terminals.

To download the latest archive with the GitHub CLI, use `gh release download --repo yannelli/pi-rust --pattern '*<target>*'`. If macOS stops `pi` after a browser download, run `xattr -d com.apple.quarantine pi`.

Each release file has a signed build provenance attestation. To make sure that a file came from the Release workflow of this repository, run `gh attestation verify pi-$tag-$target.tar.gz --repo yannelli/pi-rust`. On Windows, use the `.zip` file name.

`pi update --self` replaces a release installation with the latest release. `pi update --models` updates the model catalogs. To build from source, you need Rust 1.85 or newer. Then run `cargo install --path crates/pi-coding-agent`.

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

Run `pi --help` to see all options. In interactive mode, type `/` to see the list of commands, for example `/login`, `/model`, `/settings`, `/tree`, `/fork`, `/compact`, `/export`, `/share`, `/mcp`, and `/hotkeys`. If you type while the agent works, pi puts your input in the queue as steering. Alt+Enter puts it in the queue as a follow-up. On Windows, use Ctrl+Q.

`/diff [--stat] [path]` (pi-rust extension) shows the uncommitted git changes in the working directory. It shows the `git diff HEAD` stat, the untracked files, and the diff of the staged and unstaged changes. With `--stat`, it does not show the diff. With a path, it shows only the changes to that path. It shows a maximum of 500 lines, and a note gives the number of lines that it did not show.

`/context` (pi-rust extension) shows how much of the context window of the current model the conversation uses. It shows the model, the window size, the tokens in use and their percentage, and the point where auto-compaction starts. The count of tokens in use comes from the usage that the last response reported, plus estimates for newer messages. Before a response reports usage, the count is an estimate of the next request. Then a table shows the estimated tokens for each part of the context and the free space. The parts are the system prompt, tool definitions, context files, skills, user messages, assistant messages, and tool results.

`/export [file]` writes the session to an HTML file. If the file name ends in `.jsonl`, it writes JSONL. `pi --export <session.jsonl> [output]` exports a saved session file to HTML. With both commands, an output path that ends in `.md` gives a Markdown file (pi-rust extension).

The Markdown file contains the current branch. It starts with a title, the date, the models, the working directory, the tokens, and the cost. Then it gives each user turn and assistant turn. Thinking is in collapsed `<details>` blocks, and tool calls show their JSON arguments. Tool results stop after 200 lines, and images change to placeholders.

`/init [instructions]` (pi-rust extension) tells the agent to look through the project in the working directory. Then the agent writes an `AGENTS.md` file there, based only on what it finds. The file contains an overview, the build, test, and lint commands, the code layout, the conventions, and the known problems. If the directory already has a context file that pi loads, the agent improves that file. These context files are `AGENTS.md`, `CLAUDE.md`, and `AGENTS.override.md`.

pi adds the text after `/init` to the prompt as more instructions. The prompt is a normal message. It shows in the chat and uses the current model. pi saves it in the session. As with other built-in commands, `/init` has priority over a prompt template or an extension command with the name `init`. You can use `/init` only in interactive mode.

`/review [instructions]` (pi-rust extension) tells the agent to review the uncommitted changes in the working directory. The prompt tells the agent to run `git status` and `git diff HEAD` and to read the untracked files. The agent then reports bugs, risky changes, and missing tests in order of severity, with `path:line` references, and it does not change files. pi adds the text after `/review` as more instructions, for example an area to look at or a base branch for the comparison. Outside a git repository, `/review` shows an error and sends nothing. As with `/init`, the prompt is a normal message.

`/undo` (pi-rust extension) restores the files that the edit and write tools changed during the last prompt. It writes back the earlier content of these files and deletes the files that the agent created. To go back one more prompt, run `/undo` again. A file can change after the agent writes it, for example when you or a bash command change it. `/undo` does not restore such a file and shows it in a list. `/undo` does not change the conversation, but you can use `/tree` to move in it.

`/undo` has these limits:

- It records only the changes from the edit and write tools, not the changes from bash commands.
- It keeps checkpoints in memory for the last 20 prompts of the current session.
- It does not keep files that are larger than 4 MB.
- It loses all checkpoints when pi stops or when you change to a different session.

`/retry` (pi-rust extension) runs the last prompt of the current branch again. It moves to the point before that prompt, as `/tree` does, and sends the same text and images as a new prompt. The new answer starts a new branch, and you can go back to the old answer with `/tree`. `/retry` ignores custom messages and prompts from extensions, so it uses the last prompt that you sent. pi knows which prompts came from extensions only while the session that received them is open, and not after a resume. If the agent works or compacts, or if there is no prompt yet, `/retry` shows an error and sends nothing.

### Prompt history (pi-rust extension)

The interactive editor keeps your prompts from all sessions and projects in `~/.pi/agent/prompt-history.jsonl`. The file has one JSON string on each line. On Unix, only you can read the file. Up and Down move through the prompts, newest first. If a prompt starts with a space, pi does not save it in the file.

To search the history:

1. Press Ctrl+R to start a reverse search.
2. Type text to filter the prompts. The search ignores case.
3. To go to the next older match, press Ctrl+R again.
4. To put the match in the editor, press Enter. pi does not send it.

To go back to your draft, press Esc.

To turn off the history, set `"promptHistory": {"enabled": false}` in `settings.json`. To keep a different number of prompts, set `"maxEntries"`. The default is 1000. To use a different key for the search, set `tui.editor.historySearch` in `keybindings.json`.

## Shell completion and `pi doctor` (pi-rust extension)

`pi completion bash|zsh|fish|powershell` prints a completion script for options and subcommands. The script also completes file paths. For `--model` and `--provider`, the script gets the models of the built-in catalog and `models.json` from pi. `pi completion --help` shows how to install each script. For example:

```sh
echo 'eval "$(pi completion bash)"' >> ~/.bashrc
pi completion fish > ~/.config/fish/completions/pi.fish
```

`pi doctor` prints a short report with these items:

- The version and the target.
- The installation method and path.
- Other `pi` programs that come earlier on `PATH`, for example the npm pi.
- Errors in `settings.json`, `models.json`, `auth.json`, and `mcp.json`.
- The providers that have credentials. The report shows their names and sources, but never the secrets.
- Model Context Protocol (MCP) servers whose commands are missing.
- The features that the terminal supports.

`pi doctor --json` prints the same report as JSON. `pi doctor` does not use the network and does not start MCP servers. If it finds a problem, it exits with code 1.

## Sessions and usage stats (pi-rust extension)

`pi sessions` lists the saved sessions of the current project, newest first. For each session, it shows a short ID, the last activity, the number of messages, the cost, and the name or the first prompt. `--all` lists the sessions of all projects. `--limit <n>` shows the newest n sessions. `--json` prints an array.

`pi sessions rm <id-prefix|path>...` deletes sessions by ID prefix or by file path. If a prefix matches more than one session, the command deletes nothing and lists the matches. `pi sessions prune --older-than 30d` deletes the sessions that had no activity for that time. The units are `s`, `m`, `h`, `d`, and `w`. The command also prints the number of bytes that it frees. With `--dry-run`, it shows the sessions that it can delete and does not delete them.

On a terminal, both commands ask before they delete. In other cases, they need `--yes`.

`pi stats` adds up the sessions of the current project, or of all projects with `--all`. It shows the number of sessions, prompts, and assistant turns. It also shows the input tokens, output tokens, cache tokens, and cost. It gives these totals for each model and provider, and for each day of the last 14 days. `--since 7d` counts only the recent activity, and `--json` prints the report as JSON.

`pi stats` and `pi sessions list` only read session files, and no session command uses the network. All session commands obey `--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, and the `sessionDir` setting.

```sh
pi sessions --all --limit 10
pi sessions prune --older-than 30d --dry-run
pi stats --since 7d
```

## Run limits (pi-rust extension)

`--max-turns <n>` stops a run after n assistant responses to one prompt. Tool results do not count, and the count starts at zero for each prompt. `--max-cost <usd>` stops the run when the total cost of the session reaches the limit. This total is the same as the cost in the footer. After the cost limit stops a run, a new prompt does not start a run. Both limits apply in all modes, but only for the current process.

A limit stops the run in the same way as Esc, after the tool results of the last turn. Thus the session file stays valid, and `pi -c` can continue it. If a run ends by itself on its last turn under the limit, pi does not stop it.

```sh
pi -p --max-turns 20 --max-cost 0.50 "Fix the failing tests"
echo $?   # 3 when a limit stopped the run
```

In print mode, pi writes a line such as `pi: stopped after 20 turns (--max-turns)` to stderr. Then it does not print the remaining messages, and it exits with code 3. `--mode json` does the same. JSON mode and remote procedure call (RPC) mode also send a `run_limit` event before `agent_end`:

```json
{"type":"run_limit","limit":"maxTurns","max":20,"turns":20,"cost":0.1234,"message":"stopped after 20 turns (--max-turns)"}
```

The event has these fields:

- `limit`: `maxTurns` or `maxCost`.
- `max`: the limit.
- `turns`: the number of responses in the run.
- `cost`: the total cost of the session in US dollars.

If the cost limit stops a prompt before its run starts, pi sends only the event, and an RPC `prompt` gets the disposition `handled`. Interactive mode shows the message as a status line. If an extension registers its own `--max-turns` or `--max-cost` flag, the extension gets the value, as in upstream pi.

## Structured output (pi-rust extension)

`--output-schema <file>` makes the final answer in print mode a JSON value that matches a JSON Schema, so that scripts can read it. pi adds the schema to the system prompt. It checks the last response with the same validator that it uses for tool arguments. pi accepts one code fence around the JSON, such as ```` ```json ````. If the response is not valid JSON or does not match the schema, pi sends the errors back in the same session. Then it asks for correct JSON.

`--output-retries <n>` sets the number of correction prompts that pi sends. The default is 1, and `0` sends none.

```sh
pi -p --output-schema review.schema.json "Review the diff in HEAD" > review.json
echo $?   # 4 when the answer still does not match
```

If the answer matches the schema, text mode prints only the JSON value, on one compact line. `--mode json` ends with an `output` event:

```json
{"type":"output","value":{"verdict":"approve","issues":[]}}
```

If the answer still does not match after the retries, pi writes the errors to stderr and exits with code 4. In this case, text mode prints nothing to stdout. After an error or an aborted response, pi exits with code 1. After a run limit, it exits with code 3, as in [Run limits](#run-limits-pi-rust-extension). Before pi starts, it checks the schema file. The file must be valid JSON, use only local `$ref`s, and use only keywords and patterns that the validator supports.

The flag works only in print mode: with `-p`, with `--mode json`, or when stdin or stdout is not a terminal. Interactive mode rejects it. RPC mode and Agent Client Protocol (ACP) mode also reject it. If an extension registers its own `--output-schema` or `--output-retries` flag, the extension gets the value.

## Status line (pi-rust extension)

The `statusLine` setting replaces the two footer lines with the cwd, the token counts, and the model. Lines from extension `setStatus` calls still show below it, and an extension `setFooter` still replaces the whole footer. `/statusline` shows the active status line, and `/statusline <choice>` switches it in `~/.pi/agent/settings.json`.

| `type` | What it shows |
| --- | --- |
| `builtin` (the default) | Segments that pi-rust draws itself. It starts no process. |
| `command` | The output of your command, as in the `statusLine` setting of Claude Code and the Cursor CLI. |
| `claude` | The `statusLine` command in `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`). |
| `cursor` | The `statusLine` command in `~/.cursor/cli-config.json`. |
| `codex` | The `[tui] status_line` items in `~/.codex/config.toml` (or `$CODEX_HOME/config.toml`), drawn as Codex draws them. |
| `off` | The default footer. |

With `claude`, `cursor`, or `codex`, you can use the status line that you already set up for that agent. Run `/statusline from-claude`, `/statusline from-cursor`, or `/statusline from-codex` to switch to it.

### Builtin status lines

Pick a preset, then change its parts if you want:

```json
{ "statusLine": { "preset": "powerline", "theme": "catppuccin", "icons": "nerd" } }
```

| Preset | Look |
| --- | --- |
| `minimal` (the default) | One line of text in pi's theme colors: the model, thinking level, context, and cost, with the folder and branch at the right |
| `pi-rust` | Blocks in the pi-rust logo colors, with a π |
| `pi` | Blocks in the colors of the upstream pi logo |
| `claude` | Blocks in Claude colors |
| `codex` | The Codex status line: model and reasoning, context left, cwd, and branch, joined by `·` |
| `cursor` | The Cursor CLI footer: model and context use, with the cwd and branch at the right |
| `pills`, `powerline` | Blocks or Powerline arrows in the pi-rust colors |
| `rounded`, `slanted` | Round or slanted blocks with Nerd Font icons, in `catppuccin` and `tokyo-night` |
| `retro`, `dots`, `emoji`, `mono` | Brackets in `gruvbox`, dots in `nord`, emoji icons in `catppuccin`, and gray blocks |
| `full` | Two lines with every common segment |

| Field | Values |
| --- | --- |
| `chip` | The shape of each segment: `plain` (colored text), `block` (text on a colored block), `rounded`, `powerline`, `slanted`, `brackets` (`[text]`), or `dot` (`● text`). `rounded`, `powerline`, and `slanted` need a Nerd Font. |
| `theme` | `terminal` (pi's theme colors), `pi-rust`, `pi`, `claude`, `codex`, `cursor`, `dracula`, `nord`, `catppuccin`, `gruvbox`, `tokyo-night`, `solarized`, `rose-pine`, `monokai`, `one-dark`, or `mono`. Text on a block is dark or light to match the block color. On a light pi theme, light text colors are darkened. |
| `icons` | `unicode` (⎇ before the branch and ◷ before the time), `nerd` (Nerd Font icons for most segments), `emoji`, or `none` |
| `segments` | Segment names for one line, such as `["dir", "branch", "fill", "model"]`, or a list of lines, such as `[["cwd", "branch"], ["model", "contextBar"]]`. An entry can also be `{"segment": "model", "color": "#ff8800", "icon": "◆"}` or literal text, such as `{"text": "🦀", "color": "accent"}`. |
| `separator` | The text between segments. The default depends on the chip. |
| `colors` | A segment name to `#rrggbb` or to a pi theme token, such as `"accent"` |
| `padding` | Spaces before each line |

| Segment | Shows | Codex name |
| --- | --- | --- |
| `cwd`, `dir`, `project` | The cwd with `~`, its last folder, or the git repository folder | `current-dir`, `project-name` |
| `branch` | The git branch | `git-branch` |
| `session`, `sessionId` | The session name, or the first 8 characters of its ID | `thread-title`, `session-id` |
| `model`, `modelWithReasoning`, `provider`, `thinking` | The model, the model with its thinking level, the provider, or the thinking level | `model`, `model-with-reasoning`, `reasoning` |
| `tokens`, `tokensIn`, `tokensOut`, `tokensUsed` | ↑input ↓output, input, output, or all tokens of the session | `total-input-tokens`, `total-output-tokens`, `used-tokens` |
| `cache`, `cacheHit` | Cache reads and writes with the hit rate, or the hit rate | |
| `cost` | The session cost | |
| `context`, `contextUsed`, `contextRemaining`, `contextWindow`, `contextBar` | The footer context text, `3% used`, `97% left`, `500k window`, or a bar such as `▰▰▱▱▱▱▱▱ 23%`. Their color changes at 70% and 90%. | `context-used`, `context-remaining`, `context-window-size` |
| `runState` | `idle`, `working`, or `compacting` | `run-state` |
| `version`, `time`, `user`, `host`, `logo` | The pi-rust version, the local time, the user, the host, or π | `codex-version` |
| `fill` | Moves the segments after it to the right edge | |

A segment with no value, such as `branch` outside a git repository, does not show. pi ignores unknown names, so Codex items that pi does not know, such as `weekly-limit`, do not show. If the right side does not fit, pi cuts it from the end.

### Command status lines

```json
{ "statusLine": { "type": "command", "command": "~/.pi/agent/statusline.sh", "padding": 0, "refreshInterval": 5 } }
```

```sh
#!/bin/sh
jq -r '"\(.model.id) \(.context_window.used_percentage // 0 | floor)% $\(.cost.total_cost_usd)"'
```

pi runs the command in the bash shell that the `bash` tool uses. It writes a newline-terminated JSON object with the session data to the command's stdin, and it shows each line that the command prints. ANSI colors and OSC 8 links work. `COLUMNS` gives the terminal width.

- The JSON uses the Claude Code field names where pi has the same data: `session_id`, `session_name`, `transcript_path` (the session file), `cwd`, `workspace.current_dir`, `model.id`, `model.display_name`, `version`, `cost.total_cost_usd`, `thinking.enabled`, and `context_window` (`total_input_tokens`, `total_output_tokens`, `context_window_size`, `used_percentage`, `remaining_percentage`). Scripts using these fields can be reused. Scripts that parse Claude's transcript format or require Claude-only fields need adaptation. pi also sends `model.provider`, `git.branch`, `git.project`, `run_state`, `routed_model`, `tokens` (`input`, `output`, `cache_read`, `cache_write`, `cache_hit_percentage`), `thinking.level`, `cost.subscription`, and `context_window.auto_compact`.
- pi runs the command when the session starts, and 300 ms after the data or the terminal width changes. A new run stops a run that has not finished. `refreshInterval` (at least 1 second) also runs the command on a timer.
- Until the first run finishes, and when a run fails or takes more than 10 seconds, pi shows the last output. If there is no output yet, it shows the default footer. pi shows a maximum of 8 lines.
- pi reads `statusLine.command` only from `~/.pi/agent/settings.json`. A project's `.pi/settings.json` can change the other fields, but it cannot set a command.

## iTerm2 integration (pi-rust extension)

The iTerm2 integration is off by default. `/terminal-setup` turns it on, or set `terminal.iterm2` in `settings.json`:

```json
{ "terminal": { "iterm2": true } }
```

When pi runs in iTerm2, the integration does these things:

| Feature | Key | What it does |
| --- | --- | --- |
| Tab color | `tabColor` | The tab is rust orange while the agent works, copper while a dialog waits for you, green when a run is done, and red after an error. Idle sessions and exit restore the profile color. |
| Badge | `badge` | The badge shows `π` and the session name, or the folder name when the session has no name. Exit removes it. |
| User variables | `userVars` | pi sets `user.pi_state` (`idle`, `working`, `blocked`, `done`, or `error`), `user.pi_session`, `user.pi_project`, and `user.pi_model`. Show them in a status bar component, a badge, or a tab title, for example `\(user.pi_state)`. |
| Attention | `attention` | When a run takes `terminal.notifyMinSeconds` or more (10 by default) and the terminal is not known to have focus, iTerm2 bounces the Dock icon. |
| Current directory | `cwd` | iTerm2 learns the cwd of the session, so a new tab can open in it. |

`true` turns on all features. In an object, a feature that you do not name is on, so `{ "badge": false }` turns on all features except the badge. pi finds iTerm2 through `TERM_PROGRAM`, `LC_TERMINAL`, or `ITERM_SESSION_ID`, so the integration also works in tmux and over SSH. In tmux, run `tmux set -g allow-passthrough on` so that the sequences reach iTerm2.

`/terminal-setup` (as in Claude Code) does these steps in iTerm2:

- It sets `terminal.iterm2` to `true` in `~/.pi/agent/settings.json` when unset or `false`. An existing feature-selection object is preserved.
- On the local Mac (not over SSH), it lets programs write the clipboard (`defaults write com.googlecode.iterm2 AllowClipboardAccess -bool true`), so `/copy` works. Restart iTerm2 to apply this change.
- If [tmc/it2](https://github.com/tmc/it2) is installed, it offers to configure Option-as-Alt and escape-sequence notification alerts for the current pane. Run `it2 auth check` first and approve API access. Pi asks before changing the pane and leaves saved profiles untouched.
- It provides manual steps when automation is unavailable, and instructions for a status bar component showing `user.pi_state`. Pi does not install external utilities automatically.

In other terminals, `/terminal-setup` changes nothing.

## ACP (Zed and other editors)

`pi --mode acp` runs pi as an [Agent Client Protocol](https://agentclientprotocol.com) server over stdio. Editors such as Zed can then use pi as an external agent. In Zed, add this to `settings.json`:

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

Each editor session runs a pi session in its project directory. pi saves it in `~/.pi/agent/sessions/`, as it does all sessions. Thus the editor can show these sessions in a list and resume or delete them. `pi -r` can resume them too. These features of the editor work: the model and thinking selectors, tool call diffs and terminals, context usage, and slash commands. The slash commands are prompt templates, `/skill:name`, `/compact`, `/autocompact`, `/export`, `/session`, `/name`, `/steering`, `/follow-up`, and `/changelog`.

The **Mode** selector controls tool approval independently of **Thinking**:

- **Auto** (default): allow tools marked read-only; ask before other tools.
- **Ask**: ask before every tool call.
- **Bypass**: skip ACP tool approvals. Extension dialogs still apply.

Approval failures, rejected requests, and cancellation prevent execution. These modes are not a sandbox: Auto trusts tool authors' read-only annotations, and read-only tools can read outside the project. Use Ask when each call needs review. New connections start in Auto; Bypass is not saved. Model labels use provider display names, and catalog variants with a `fast` segment in their ID have a ⚡ label. ACP has no standard model icon field; this label does not add a fast-mode toggle to other models.

Paseo's error `cannot preapprove exact MCP tools for unattended execution` is a host-side provider-policy restriction, raised before ACP starts. Changing Pi's mode does not remove it; the Paseo ACP adapter needs exact-tool preapproval support. Do not use Bypass as a substitute for an exact-tool allowlist. Client-supplied `mcpServers` are not currently loaded; configure servers in Pi's `mcp.json` files.

To hide the startup information block, set `quietStartup: true` in the pi settings. To accept embedded file contents in prompts, set `PI_ACP_ENABLE_EMBEDDED_CONTEXT=true`. If no models are configured, the editor offers to start pi in a terminal, where you can log in. ACP mode is a port of [pi-acp](https://github.com/svkozak/pi-acp) by Sergii Kozak (MIT license).

## Configuration

pi-rust reads the same files as upstream pi, so the two can use one setup. `PI_CODING_AGENT_DIR` sets a different folder in place of `~/.pi/agent`.

| Path | Purpose |
| --- | --- |
| `~/.pi/agent/auth.json` | API keys and OAuth logins from `/login` or `pi auth` |
| `~/.pi/agent/settings.json` | Settings. In a trusted project, `<project>/.pi/settings.json` overrides them |
| `~/.pi/agent/models.json` | Custom providers and models |
| `~/.pi/agent/keybindings.json` | Key bindings |
| `~/.pi/agent/mcp.json`, `<project>/.pi/mcp.json` | MCP servers |
| `~/.pi/agent/{extensions,skills,prompts,themes}/` | Extensions, skills, prompt templates, and themes. `.pi/` in a project and `~/.agents/skills` also work |
| `AGENTS.md`, `CLAUDE.md`, `SYSTEM.md`, `APPEND_SYSTEM.md` | Context files and system prompt overrides |
| `~/.pi/agent/sessions/` | Sessions as JSONL trees in the pi format |
| `~/.pi/agent/prompt-history.jsonl` | The prompts that you sent, for Up, Down, and Ctrl+R (pi-rust only) |

When pi-rust starts for the first time, it moves files from older pi layouts, as upstream pi does. For example, it moves `oauth.json` into `auth.json`.

## Providers

pi-rust has all the wire protocols of upstream pi: `anthropic-messages`, `openai-responses`, `openai-completions`, `openai-codex-responses` (SSE and WebSocket), `azure-openai-responses`, `google-generative-ai`, `google-vertex`, `bedrock-converse-stream`, `mistral-conversations`, and `pi-messages`. The bundled model catalog is the catalog of upstream pi, and `pi update --models` updates it. `/login` supports OAuth for Anthropic, OpenAI Codex, GitHub Copilot, OpenRouter, xAI, Kimi, Meta, and Radius. pi also accepts API keys and the default AWS and Google credentials of your system. llama.cpp servers and virtual models work as in upstream pi.

### Disable providers globally

Add exact provider IDs to `~/.pi/agent/settings.json` (or the directory set by `PI_CODING_AGENT_DIR`):

```json
{
  "disabledProviders": ["openrouter", "google-antigravity"]
}
```

Restart Pi or its ACP connection after editing. These providers' models are excluded from model selection, including ACP, RPC, the TUI, CLI listings, and completions. The filter survives catalog refreshes and applies to custom and extension models too. Project settings cannot override it. Credentials and cached catalogs are kept; remove an ID to enable its models again.

### OpenRouter

To log in, use `/login openrouter` or set `OPENROUTER_API_KEY`.

With a key, pi lists only the OpenRouter chat models that the key can use. For each model, it shows the context size, maximum output, and prices for that key. This data comes from the `/models/user` endpoint of OpenRouter, so it includes the guardrails and provider limits that you set in OpenRouter. pi does not filter image and classifier models.

pi updates the list in the background at these times:

- When it starts in interactive, RPC, or ACP mode.
- When `/model` opens.
- After `/login`.
- When you run `pi update --models`.

pi saves the last list in `models-store.json`. It uses this list offline, in `pi -p`, and until an update finishes. If you change keys outside `/login`, pi keeps the list of the old key until the next update.

Regional endpoints show only the models of their region. To use a regional endpoint, set the base URL in `models.json`:

```json
{
  "providers": {
    "openrouter": { "baseUrl": "https://us.openrouter.ai/api/v1" }
  }
}
```

If the base URL rejects the key at `/models/user`, or does not have that endpoint, pi lists all OpenRouter models. A proxy that you set as the OpenRouter base URL is an example.

### Account model lists (pi-rust extension)

pi-rust also limits other providers to the models that your account or key can use, as it does for OpenRouter. Upstream pi does this only for OpenRouter. With a credential, pi gets the model list of the provider a maximum of one time in 4 hours. It also gets the list immediately after `/login` and when you run `pi update --models`.

When pi gets the list, it does these steps:

- It removes the catalog chat models that the list does not name.
- It uses the context size and maximum output from the list, if the list has them.
- It adds the listed chat models that the catalog does not have. Each one is a copy of the nearest catalog model.
- If the list does not name the default model of the provider, pi removes no models. It only uses the limits and adds models.

pi does not filter image and classifier models. pi saves the list in `models-store.json` with the pi version, but without the credential. The base URL that you set in `models.json` applies. After an upgrade, pi shows the full catalog until a new list arrives. Model list requests do not follow redirects, so pi sends the credential only to the configured host.

To get the full catalog, set the `baseUrl` of the provider in `models.json` to an endpoint that has no model list, such as a proxy. pi shows all catalog models when the model list answers 401, 403, or 404, or when pi cannot read its body. If the list is empty or names no catalog chat model, pi shows the full catalog and does not save the list. After other failures, redirects too, pi keeps the last saved list.

#### OpenAI Codex

With a ChatGPT login, pi reads the models of the account from the Codex backend at `<base URL>/codex/models`. This is the list that Codex shows, and pi skips hidden models. This endpoint is not a public API, so pi accepts some changes in its format. For example, pi uses `id` when `slug` is missing.

#### Anthropic

With an API key or a Claude subscription login, pi reads `GET /v1/models` with the authentication headers of the provider. It reads all pages of the list. The list gives the context size and maximum output of each model.

#### GitHub Copilot

With a Copilot login, pi reads `GET /models` from the Copilot endpoint of the login. It keeps the chat models that support tools, that the model picker shows, and that no policy disables. `/login` selects models with the same rules. The endpoint of the token has priority over the `models.json` base URL, as it does for requests.

#### OpenAI and xAI

With an API key, pi reads `GET <base URL>/models`. This list also names embedding, audio, image, moderation, and realtime models. Thus pi adds only listed models of a model family that the catalog has, such as `gpt-` or `grok-`. It never adds models of those other types. The list gives no limits, so pi keeps the limits from the catalog.

With a ChatGPT login for `openai`, the same request returns the models of the account by `slug`. pi uses the `context_window` of each model as its context size. It does not use the larger `max_context_window`.

#### Google

With a Gemini API key, pi reads `GET <base URL>/models` with `x-goog-api-key` and reads all pages. It uses only the models that support `generateContent`, with their input and output token limits. pi adds new models only for a model family that the catalog has.

#### Not covered

Cursor and Grok Build do not give a public model list API to other harnesses, so pi-rust does not limit their models. Other providers show the full catalog.

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

## Differences from upstream pi

pi-rust has all the features of the upstream `pi` CLI and its libraries. These parts are different:

- Extensions run on an embedded QuickJS engine, so you do not need Node.js. All Node built-in modules load. These include:
  - `zlib` with gzip, brotli, and zstd.
  - `async_hooks` with `AsyncLocalStorage`, and `v8` serialization.
  - The network modules `net`, `tls`, `dgram`, `dns`, `http`, `https`, and `http2`.
  - `vm` contexts with timeouts, `worker_threads`, and `cluster`, which can send socket and server handles over inter-process communication (IPC).
  - `node:sqlite` on the bundled SQLite, `node:test` with TAP and spec output, and `repl` over any streams.
  - In-process `inspector` sessions, and `wasi` with a `WebAssembly` global on the wasmi interpreter.

  The APIs that do not work throw `ERR_PI_UNSUPPORTED`. Examples are `crypto.subtle` other than `digest`, and rare `crypto` ciphers such as Camellia. `crates/pi-js/src/unsupported.rs` lists all of them. Native npm addons cannot load.
- `grep` and `find` use the Rust `ignore` crate, so pi-rust does not download `rg` or `fd`.
- The version check and `pi update --self` use the releases of this repository.
- Word movement in Chinese, Japanese, Korean (CJK), and Thai text uses ICU4X dictionaries. Thus some word boundaries are different from `Intl.Segmenter` in Node.
- `pi --mode acp` (see [ACP](#acp-zed-and-other-editors)) is an addition. Upstream pi needs the separate pi-acp adapter. Permission modes are separate from thinking levels and gate tool execution.
- Global `disabledProviders` settings hide whole providers from model selection without deleting credentials.
- Idle Escape clears the input draft (undo restores it). Submitted slash commands join prompts and shell commands in Up/Down history.
- Markdown export (see [Use](#use)) is an addition: `/export notes.md` and `pi --export session.jsonl notes.md` write Markdown. Upstream pi exports only HTML and JSONL.
- The slash commands `/diff`, `/init`, `/review`, `/context`, `/undo`, and `/retry` (see [Use](#use)) are additions. Upstream pi has none of them, and it shows context use only in the footer.
- `pi completion` and `pi doctor` are additions. Upstream pi has neither command.
- `pi sessions` and `pi stats` are additions. Upstream pi lists and deletes sessions only in its session picker (`pi -r` and `/resume`). It shows usage only for the current session (`/session`).
- Run limits (`--max-turns`, `--max-cost`) and structured output (`--output-schema`, `--output-retries`) are additions.
- Account model lists for OpenAI Codex, Anthropic, GitHub Copilot, OpenAI, xAI, and Google (see [Providers](#account-model-lists-pi-rust-extension)) are an addition. Upstream pi limits only OpenRouter.
- pi-rust fixes these bugs that are still open upstream:
  - If you remove a tool from the first tool list, later Anthropic requests no longer fail (upstream issue 11376).
  - On Bedrock, `strict: "prefer"` tools change to non-strict when the strict tools have more than 16 optional parameters (11416).
  - `modelOverrides` in `models.json` also apply to classifier and image models: `name`, `input`, `inputLimits`, and `cost`, plus `contextWindow` and `compat` for classifiers (11223).
  - The editor removes escape sequences from pasted text. If the end marker of a paste does not arrive, the paste ends after 10 seconds without input or at 16 MiB. Then Ctrl+C works again (11383).
  - MCP HTTP replies that are plain JSON obey `maxMessageBytes` (11335).
  - A code mode script can declare its own top-level `tools` or `console` (11226).
  - Branch summaries from `/tree` use the selected thinking level (11444). If you resume a session after `/tree`, pi restores the thinking level that you used last (11451).
  - pi removes mouse reports and other escape sequences with private parameters, such as `ESC[<35;59;1M`, from tool output (11377).
  - In `pi -p`, a prompt that an extension command sends with `sendUserMessage` runs before pi exits (11238).
  - An unpinned `-e npm:` package updates when npm has a newer version (11375).
  - `pi.exec` keeps a maximum of 16 MiB of output from a command, and stops the command when the output reaches that limit (11360).
  - `pi-env` watches a directory again after the kernel drops the watch and the directory keeps its inode (11342).
- [Prompt history](#prompt-history-pi-rust-extension) across sessions, with the Ctrl+R search and the `promptHistory` settings, is an addition. Upstream pi keeps prompt history only for the running process.
- `pi-env` looks for remote daemons in a `pi-env-daemons` folder next to the running program. Each release has a `pi-env-daemons-<tag>.tar.gz` archive with the daemons for Linux, Android (Termux), macOS, and Windows on x86-64 and arm64. Extract this archive in the folder that contains `pi`.
- Notifications: in interactive mode, when a run takes 10 seconds or more, pi sends a terminal notification at the end. Its title is "pi", and it says "Done" or gives the error. Upstream pi has this only as the example extension `notify.ts`.
  - pi sends OSC 777 in Ghostty, WezTerm, foot, and rxvt, OSC 99 in kitty, and OSC 9 in iTerm2 and Windows Terminal. In other terminals, for example tmux and screen, it sends BEL.
  - In fullscreen mode, pi does not send the notification while the terminal reports focus.
  - Set `terminal.notifyOnFinish` to `"auto"` (the default), `"osc"`, `"bell"`, or `"off"`. Set `terminal.notifyMinSeconds` to change the time limit.
- The [status line](#status-line-pi-rust-extension) (`statusLine`) is an addition. Upstream pi changes the footer only through extensions.
- The [iTerm2 integration](#iterm2-integration-pi-rust-extension) (`terminal.iterm2`) and `/terminal-setup` are additions.
- `pi experimental` and its commands `server`, `client`, `durable`, and `vacation` need `PI_EXPERIMENTAL=1`, as in upstream pi. `server` and `client` use Unix sockets, so on Windows they show a message that they need Unix. Upstream pi also has no Windows transport.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The tests use the `faux` provider and local mock HTTP servers. They do not need API keys or network access.

## Releases

Versions obey [Semantic Versioning](https://semver.org/). The commit messages decide the next version, as in [Conventional Commits](https://www.conventionalcommits.org/):

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

If you push a `vX.Y.Z` tag that matches `Cargo.toml`, the workflow releases that version. A `vX.Y.Z-rc.1` tag publishes a pre-release. To build all targets without a release, run the workflow manually with `dry_run`. To release when the commits do not need a release, run the workflow manually. Set `bump` to `patch`, `minor`, or `major`, and turn off `dry_run`.

```sh
cargo xtask plan                 # next version and release decision
cargo xtask prepare 0.2.0        # set the version and update CHANGELOG.md
cargo xtask notes 0.2.0          # print release notes
```

## License

MIT. pi-rust is based on [pi](https://github.com/earendil-works/pi) by Mario Zechner. See [LICENSE](LICENSE).

The ACP mode is a port of [pi-acp](https://github.com/svkozak/pi-acp) by Sergii Kozak, also under the MIT license. Its license is in [`LICENSE-pi-acp`](crates/pi-coding-agent/src/acp/LICENSE-pi-acp), and each release archive includes it.

Some JavaScript modules of the extension runtime are ported from [Node.js](https://github.com/nodejs/node) (MIT). The MCP OAuth code is ported from upstream pi, which adapted it from the [MCP TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk) (MIT). Their licenses are in [`LICENSE-node`](crates/pi-js/src/js/LICENSE-node) and [`LICENSE-mcp-typescript-sdk`](crates/pi-mcp/src/oauth/LICENSE-mcp-typescript-sdk), and each release archive includes them.

The release archives also include `THIRD-PARTY-LICENSES.md`, with the licenses of the third-party Rust crates in the binaries. The Release workflow writes it with [cargo-about](https://github.com/EmbarkStudios/cargo-about) from `about.toml` and `about.hbs`.
