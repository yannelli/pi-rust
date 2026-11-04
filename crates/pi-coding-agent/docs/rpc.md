# RPC mode

`pi --mode rpc` reads JSON commands on stdin and writes responses and session events on stdout, one JSON record per line, as upstream pi does. This page lists what pi-rust adds to the protocol. Upstream clients can ignore the additions.

## Built-in commands

A `prompt` whose message is one of these commands runs in pi without a model call:

| Command | Effect |
| --- | --- |
| `/usage [provider]` | The subscription limits of the logged-in providers, as `pi usage` shows them. |
| `/context` | How the conversation uses the context window of the current model, as the TUI `/context` shows it. |
| `/extensions [list]` | The configured packages and extensions with their enabled state, as `pi extensions list` shows them. `-l` and `--approve` work as in the CLI. |
| `/extensions status` | The extensions this session loaded, their load errors, and the built-in extensions on or off. |
| `/extensions enable <name-or-path>`, `/extensions disable <name-or-path>` | Saves the change as `pi extensions enable` and `disable` do, then reloads the session's extensions and reports the result. |
| `/fast [on\|off]` | Sets fast mode or toggles it, and saves the choice as `fastMode`. The output says when the current model has no fast mode. |
| `/reload` | Reloads extensions, skills, prompts, and context files. It refuses while a response or a compaction runs. |
| `/session` | Session ID, file, message count, cost, and tokens. |
| `/changelog` | The pi changelog, cut after 20,000 characters. |
| `/export` | Writes the session as `pi-session-<id>.html` in the session working directory. |
| `/name <name>` | Sets the session display name. |

The command writes its output as a `command_output` record before the `prompt` response:

```json
{"type": "command_output", "text": "Fast mode on"}
{"id": "1", "type": "response", "command": "prompt", "success": true, "data": {"disposition": "handled", "agentInvoked": false}}
```

The text is plain text that also reads as Markdown. Tables, such as the `/context` and `/usage` reports, are fenced code blocks.

A `/reload` or an extension change binds the extensions again. Extensions from `--extension`, extension dialogs, and the `/mcp` command keep working.

`get_commands` lists these commands after `/mcp` and before the extension commands, in the shape of the `/mcp` entry: `source` is `extension`, and `sourceInfo.path` is `builtin:<name>` with `sourceInfo.source` `builtin`.

## agentInvoked

The `prompt` response data has `agentInvoked` when pi knows whether the prompt reached the model:

| Prompt | `disposition` | `agentInvoked` |
| --- | --- | --- |
| A built-in command | `handled` | `false` |
| An extension command, `/mcp`, or an input handler that consumed the prompt, while no run is active | `handled` | `false` |
| A prompt that started a run | `started` | `true` |
| A prompt queued as steering or a follow-up | `queued` | absent |

A client can finish the turn at once when `agentInvoked` is `false`.

## Other additions

- `set_fast_mode` and the `fastMode` and `fastModeAvailable` fields of `get_state`. See [Fast mode](providers.md#fast-mode-pi-rust-extension).
- The `run_limit` event. See [Run limits](cli.md#run-limits).
