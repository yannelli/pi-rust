# Status line

The `statusLine` setting replaces the two footer lines with the current working directory (cwd), the token counts, and the model. Lines from extension `setStatus` calls still show below it. An extension `setFooter` still replaces the whole footer.

## Edit the status line

- To open the editor, run `/statusline` or `/statusline edit`.
- To change the preset in `~/.pi/agent/settings.json`, run `/statusline <choice>`.

The editor sets presets, palettes, pill shapes, icons, alignment, separators, padding, and context displays. In **Modules / order**, you can add, remove, and reorder modules. You can also move modules between rows.

- To save the module edits, choose **Apply layout**. Esc discards the module edits that you did not apply.
- The other choices save immediately and update the footer.

The edits are global. Trusted project settings can override them.

Context headroom shows the available context tokens. It does not show the subscription quota of a provider.

## Types

| `type` | What it shows |
| --- | --- |
| `builtin` (the default) | Segments that pi-rust draws. It starts no process. |
| `command` | The output of your command, as in the `statusLine` setting of Claude Code and the Cursor CLI. |
| `claude` | The `statusLine` command in `~/.claude/settings.json` or in `$CLAUDE_CONFIG_DIR/settings.json`. |
| `cursor` | The `statusLine` command in `~/.cursor/cli-config.json`. |
| `codex` | The `[tui] status_line` items in `~/.codex/config.toml` or in `$CODEX_HOME/config.toml`. pi draws them as Codex does. |
| `off` | The default footer. |

With `claude`, `cursor`, or `codex`, pi uses the status line that you set up for that agent. To change to it, run `/statusline from-claude`, `/statusline from-cursor`, or `/statusline from-codex`.

## Built-in status lines

Select a preset. Then change its parts if necessary:

```json
{ "statusLine": { "preset": "powerline", "theme": "catppuccin", "icons": "nerd" } }
```

| Preset | Look |
| --- | --- |
| `minimal` (the default) | One line of text in the pi theme colors: the model, thinking level, context, and cost. The folder and branch are at the right. |
| `pi-rust` | Blocks in the pi-rust logo colors, with a π |
| `pi` | Blocks in the colors of the upstream pi logo |
| `claude` | Blocks in Claude colors |
| `codex` | The Codex status line: model and reasoning, context left, cwd, and branch, joined by `·` |
| `cursor` | The Cursor CLI footer: model and context use, with the cwd and branch at the right |
| `pills`, `powerline` | Blocks or Powerline arrows in the pi-rust colors |
| `rounded`, `slanted` | Round or slanted blocks with Nerd Font icons, in `catppuccin` and `tokyo-night` |
| `retro`, `dots`, `emoji`, `mono` | Brackets in `gruvbox`, dots in `nord`, emoji icons in `catppuccin`, and gray blocks |
| `full` | Two lines with all the common segments |
| `compact`, `stacked` | A compact text row or two rows of blocks, with the remaining context percent and the token headroom |

### Fields

| Field | Values |
| --- | --- |
| `chip` | The shape of each segment: `plain` (colored text), `block` (text on a colored block), `rounded`, `powerline`, `slanted`, `brackets` (`[text]`), or `dot` (`● text`). `rounded`, `powerline`, and `slanted` need a Nerd Font. |
| `theme` | `terminal` (the pi theme colors), `pi-rust`, `pi`, `claude`, `codex`, `cursor`, `dracula`, `ghostty`, `darcula-islands`, `nord`, `catppuccin`, `gruvbox`, `tokyo-night`, `solarized`, `rose-pine`, `monokai`, `one-dark`, or `mono`. Text on a block is dark or light to match the block color. On a light pi theme, pi makes light text colors darker. |
| `icons` | `unicode` (⎇ before the branch and ◷ before the time), `nerd` (Nerd Font icons for most segments), `emoji`, or `none` |
| `segments` | Segment names for one line, such as `["dir", "branch", "fill", "model"]`, or a list of lines, such as `[["cwd", "branch"], ["model", "contextBar"]]`. An entry can also be `{"segment": "model", "color": "#ff8800", "icon": "◆"}` or literal text, such as `{"text": "🦀", "color": "accent"}`. |
| `separator` | The text between segments. The default depends on the chip. To remove extra gaps between padded pills, use `""`. |
| `layout` | `spread` (the default) obeys `fill`. `compact` ignores flexible gaps. `stacked` starts a new row at each `fill`. Empty groups do not add padding. |
| `colors` | A segment name to `#rrggbb` or to a pi theme token, such as `"accent"` |
| `padding` | The number of spaces before each line |

### Segments

| Segment | Shows | Codex name |
| --- | --- | --- |
| `cwd`, `dir`, `project` | The cwd with `~`, its last folder, or the git repository folder | `current-dir`, `project-name` |
| `branch` | The git branch | `git-branch` |
| `session`, `sessionId` | The session name, or the first 8 characters of its ID | `thread-title`, `session-id` |
| `model`, `modelWithReasoning`, `provider`, `thinking` | The model, the model with its thinking level, the provider, or the thinking level | `model`, `model-with-reasoning`, `reasoning` |
| `tokens`, `tokensIn`, `tokensOut`, `tokensUsed` | ↑input ↓output, input, output, or all tokens of the session | `total-input-tokens`, `total-output-tokens`, `used-tokens` |
| `cache`, `cacheHit` | Cache reads and writes with the hit rate, or the hit rate | |
| `cost` | The session cost | |
| `context`, `contextUsed`, `contextRemaining`, `contextWindow`, `contextBar` | The footer context text, `3% used`, `97% left`, `500k window`, or a bar such as `▰▰▱▱▱▱▱▱ 23%`. The color changes at 70% and 90%. | `context-used`, `context-remaining`, `context-window-size` |
| `contextTokens`, `contextHeadroom` | The tokens in context now, or the tokens left before the context window is full. If the usage is unknown, the segment does not show, and a bar shows `context ?`. | `context-tokens`, `context-headroom` |
| `runState` | `idle`, `working`, or `compacting` | `run-state` |
| `version`, `time`, `user`, `host`, `logo` | The pi-rust version, the local time, the user, the host, or π | `codex-version` |
| `fill` | Moves the segments after it to the right edge | |

A segment with no value does not show, for example `branch` outside a git repository. pi ignores unknown names, so Codex items that pi does not know, such as `weekly-limit`, do not show. If the right side does not fit, pi cuts it from the end.

## Command status lines

```json
{ "statusLine": { "type": "command", "command": "~/.pi/agent/statusline.sh", "padding": 0, "refreshInterval": 5 } }
```

```sh
#!/bin/sh
jq -r '"\(.model.id) \(.context_window.used_percentage // 0 | floor)% $\(.cost.total_cost_usd)"'
```

pi runs the command in the bash shell that the `bash` tool uses. pi writes a JSON object with the session data to the stdin of the command, followed by a newline. pi shows each line that the command prints. ANSI colors and OSC 8 links work. `COLUMNS` gives the terminal width.

pi reads `statusLine.command` only from `~/.pi/agent/settings.json`. A project `.pi/settings.json` can change the other fields, but it cannot set a command.

### Input JSON

The JSON uses the Claude Code field names where pi has the same data:

- `session_id`, `session_name`, `transcript_path` (the session file), `cwd`, and `workspace.current_dir`.
- `model.id`, `model.display_name`, `version`, `cost.total_cost_usd`, and `thinking.enabled`.
- `context_window`: `total_input_tokens`, `total_output_tokens`, `context_window_size`, `used_percentage`, and `remaining_percentage`.

Scripts that use these fields work without changes. Scripts that parse the Claude transcript format, or that need fields that only Claude has, need changes.

pi also sends these fields:

- `model.provider`, `git.branch`, `git.project`, `run_state`, and `routed_model`.
- `tokens`: `input`, `output`, `cache_read`, `cache_write`, and `cache_hit_percentage`.
- `thinking.level`, `cost.subscription`, and `context_window.auto_compact`.
- `context_window.remaining_tokens`. The value is null when it is unknown.

### When pi runs the command

pi runs the command at these times:

- When the session starts.
- 300 ms after the data or the terminal width changes. A new run stops a run that did not finish.
- On a timer, if you set `refreshInterval`. The minimum is 1 second.

Until the first run finishes, pi shows the last output. pi also shows the last output when a run fails or takes more than 10 seconds. If there is no output yet, pi shows the default footer. pi shows a maximum of 8 lines.
