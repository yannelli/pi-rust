# CLI commands and flags

Run `pi --help` to see all options. This page describes `pi completion`, `pi doctor`, `pi sessions`, `pi stats`, `pi extensions`, the run limits, and structured output. pi-rust adds them, and upstream pi does not have them.

## Shell completion

`pi completion bash|zsh|fish|powershell` prints a completion script. The script completes options, subcommands, and file paths. For `--model` and `--provider`, the script gets the models of the built-in catalog and `models.json` from pi.

To see how to install each script, run `pi completion --help`. Examples:

```sh
echo 'eval "$(pi completion bash)"' >> ~/.bashrc
pi completion fish > ~/.config/fish/completions/pi.fish
```

## pi doctor

`pi doctor` prints a short report with these items:

- The version and the target.
- The installation method and path.
- Other `pi` programs that come earlier on `PATH`, for example the npm pi.
- Errors in `settings.json`, `models.json`, `auth.json`, and `mcp.json`.
- The providers that have credentials. The report shows their names and sources. It does not show the secrets.
- Model Context Protocol (MCP) servers whose commands are missing.
- The features that the terminal supports.

`pi doctor --json` prints the same report as JSON. `pi doctor` does not use the network and does not start MCP servers. If it finds a problem, it exits with code 1.

## Sessions

`pi sessions` lists the saved sessions of the current project, newest first. For each session, it shows a short ID, the last activity, the number of messages, the cost, and the name or the first prompt.

| Option | Effect |
| --- | --- |
| `--all` | List the sessions of all projects. |
| `--limit <n>` | Show the newest n sessions. |
| `--json` | Print an array. |

To delete sessions, use these commands:

- `pi sessions rm <id-prefix|path>...` deletes sessions by ID prefix or by file path. If a prefix matches more than one session, the command deletes nothing and lists the matches.
- `pi sessions prune --older-than 30d` deletes the sessions that had no activity for that time. The units are `s`, `m`, `h`, `d`, and `w`. The command prints the number of bytes that it frees. With `--dry-run`, it shows the sessions that it can delete and does not delete them.

On a terminal, both commands ask before they delete. In other cases, they need `--yes`.

## Usage stats

`pi stats` adds up the sessions of the current project. With `--all`, it adds up the sessions of all projects. It shows these totals:

- The number of sessions, prompts, and assistant turns.
- The input tokens, output tokens, cache tokens, and cost.
- The totals for each model and provider, and for each day of the last 14 days.

`--since 7d` counts only the recent activity. `--json` prints the report as JSON.

`pi stats` and `pi sessions list` only read session files. No session command uses the network. All session commands obey `--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, and the `sessionDir` setting.

```sh
pi sessions --all --limit 10
pi sessions prune --older-than 30d --dry-run
pi stats --since 7d
```

## Subscription limits

`pi usage` shows the limits of the subscription logins: the 5-hour and weekly windows of Claude and ChatGPT (Codex), the billing cycle of Cursor, the weekly SuperGrok credits and Grok Build use of the `xai` login, and the quota of Google Antigravity. For each window it shows the used percentage and the time to the reset. `pi usage <provider>` shows one provider. `--json` prints `{ "providers": [...] }` in the shape of the native extension method `usage.get`. The exit code is 1 when every provider has an error.

```sh
pi usage
pi usage anthropic --json
```

[Subscription usage limits](../../../docs/providers/usage.md) lists the endpoints.

## Run limits

| Flag | Effect |
| --- | --- |
| `--max-turns <n>` | Stops a run after n assistant responses to one prompt. Tool results do not count. The count starts at zero for each prompt. |
| `--max-cost <usd>` | Stops the run when the total cost of the session reaches the limit. This total is the same as the cost in the footer. After this limit stops a run, a new prompt does not start a run. |

Both limits apply in all modes, but only for the current process. A limit stops the run in the same way as Esc, after the tool results of the last turn. Thus the session file stays valid, and `pi -c` can continue it. If a run ends by itself on its last turn under the limit, pi does not stop it.

```sh
pi -p --max-turns 20 --max-cost 0.50 "Fix the failing tests"
echo $?   # 3 when a limit stopped the run
```

When a limit stops a run in print mode or in `--mode json`, pi does these steps:

1. It writes a line such as `pi: stopped after 20 turns (--max-turns)` to stderr.
2. It does not print the remaining messages.
3. It exits with code 3.

JSON mode and remote procedure call (RPC) mode also send a `run_limit` event before `agent_end`:

```json
{"type":"run_limit","limit":"maxTurns","max":20,"turns":20,"cost":0.1234,"message":"stopped after 20 turns (--max-turns)"}
```

The event has these fields:

- `limit`: `maxTurns` or `maxCost`.
- `max`: the limit.
- `turns`: the number of responses in the run.
- `cost`: the total cost of the session in US dollars.

If the cost limit stops a prompt before its run starts, pi sends only the event. An RPC `prompt` then gets the disposition `handled`. Interactive mode shows the message as a status line. If an extension registers its own `--max-turns` or `--max-cost` flag, the extension gets the value, as in upstream pi.

## Structured output

`--output-schema <file>` makes the final answer in print mode a JSON value that matches a JSON Schema. Scripts can then read the answer. pi does these steps:

1. It adds the schema to the system prompt.
2. It checks the last response with the validator that it uses for tool arguments. It accepts one code fence around the JSON, such as ```` ```json ````.
3. If the response is not valid JSON or does not match the schema, it sends the errors back in the same session. Then it asks for correct JSON.

`--output-retries <n>` sets the number of correction prompts that pi sends. The default is 1, and `0` sends none.

```sh
pi -p --output-schema review.schema.json "Review the diff in HEAD" > review.json
echo $?   # 4 when the answer still does not match
```

The results are as follows:

- If the answer matches the schema, text mode prints only the JSON value, on one compact line. `--mode json` ends with an `output` event:

  ```json
  {"type":"output","value":{"verdict":"approve","issues":[]}}
  ```

- If the answer still does not match after the retries, pi writes the errors to stderr and exits with code 4. Text mode prints nothing to stdout.
- After an error or an aborted response, pi exits with code 1.
- After a run limit, pi exits with code 3. See [Run limits](#run-limits).

Before pi starts, it checks the schema file. The file must be valid JSON. It must use only local `$ref`s, and only keywords and patterns that the validator supports.

The flag works only in print mode: with `-p`, with `--mode json`, or when stdin or stdout is not a terminal. Interactive mode, RPC mode, and Agent Client Protocol (ACP) mode reject it. If an extension registers its own `--output-schema` or `--output-retries` flag, the extension gets the value.

## Extensions

```sh
pi extensions list                       # built-in and installed, including disabled
pi extensions search todo
pi extensions info todo
pi extensions install marketplace:todo
pi extensions install npm:<package>      # or a git URL or a local path
pi extensions disable builtin:mcp
pi extensions enable builtin:mcp
pi extensions remove npm:<package>
pi config                                # TUI to turn resources on and off
```

- If a name matches more than one extension, use the exact path from `list`.
- To change the project-local settings, add `-l`. Project changes need trust (`--approve`).
- After a change, restart pi or run `/reload`.
- Third-party extensions are executable code. Install only sources that you trust.
- Use the [marketplace](marketplace.md) to find native Rust extensions in the signed release catalog.

## Extension runtime

pi-rust runs extensions on QuickJS by default. It can also run them on Node 22 or later.

```sh
pi --extension-runtime node              # run extensions on Node
pi --extension-runtime auto              # Node only for packages that need it
PI_EXTENSION_RUNTIME=node pi             # the same, from the environment
PI_NODE_PATH=/opt/node22/bin/node pi --extension-runtime node
```

| Value | Engine |
| --- | --- |
| `quickjs` | The embedded QuickJS engine. This is the default. |
| `node` | A `node` process. |
| `auto` | Node when a loaded package sets `package.json` `pi.runtime: "node"` or ships a `.node` addon. Otherwise QuickJS. |

- pi reads the choice from `--extension-runtime`, then `PI_EXTENSION_RUNTIME`, then `extensionRuntime` in the project settings, then in the global settings.
- pi finds `node` in `PI_NODE_PATH`, then on `PATH`.
- If Node is missing, older than version 22, or does not start, pi shows one warning and uses QuickJS.

See [Node backend](../../../docs/agents/node-compat.md#node-backend) for how the backend works.
