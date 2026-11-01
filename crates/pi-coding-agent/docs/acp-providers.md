# ACP agents as providers

Created: 2026-10-30. Last updated: 2026-10-31.

pi can run an external [Agent Client Protocol](https://agentclientprotocol.com) (ACP) agent, such as Kiro CLI or Amp, and show its work in the pi transcript. This page has the acceptance criteria, the design decision, the research with sources, and the user guide. For pi as an ACP server (`pi --mode acp`), see [acp.md](acp.md).

## Acceptance criteria

1. Each enabled entry of `acpAgents` is a provider `<id>-acp` (or `<id>` when the ID ends in `-acp`) in `/model`, with the API `acp`. Selecting the provider opens its ACP session without sending a prompt, so its advertised models are available before the first turn. The `default` model keeps the agent's own model choice.
2. A request starts the agent command over stdio in the session working directory, sends `initialize` (protocol 1, `fs` and `terminal` client capabilities, `auth.terminal`), then `session/new` with the MCP servers of criterion 13. A later request of the same pi session reuses the ACP session. After a restart of pi, `session/load` resumes the ACP session when the agent advertises `loadSession`. Otherwise pi sends the earlier conversation as a context resource.
3. `session/prompt` gets the user text, images (when `promptCapabilities.image`), and a `resource_link` for each file that the user message names with `@`.
4. `session/update` goes into one assistant message: message chunks as text, thought chunks as thinking, tool calls as text lines with the kind, title, and status, with diffs as fenced `diff` blocks and terminal output when the call ends, plans as a checklist, and `usage_update` as context use. Every stream ends with `Done` or `Error` through `MessageBuilder`. A compacted transcript keeps the ACP session: pi finds the last reply of the session by its timestamp and sends only what comes after it.
5. `session/request_permission` shows a dialog with the options of the agent (allow once, allow always, reject once, reject always). Without a UI, pi rejects. A cancel answers open requests with `cancelled`.
6. `fs/read_text_file`, `fs/write_text_file`, and `terminal/create`, `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, `terminal/release` work for paths inside the session working directory. Paths outside it fail.
7. Escape sends `session/cancel`. The stream ends with stop reason `aborted`.
8. Modes (`modes` or a `configOptions` selector of category `mode`) are selectable with `/acp mode` before a prompt. Models (`configOptions` category `model`, or `models.availableModels` of older agents) are selectable in `/model`. `/thinking` uses the agent's advertised effort choices: pi level names match option IDs or labels without regard to case, and pi sends the exact advertised option ID. Unsupported levels are omitted. Options without categories use known model, permission, and effort IDs, including Kiro's `effortLevel`.
9. `/acp` opens a browser with two tabs. **Installed** lists every `acpAgents` entry of the global and the project settings. A global entry that a project entry replaces is marked and can only be enabled, disabled, or removed. The other entries can also be logged in to, set as default, and moved to the other settings file. **Registry** lists the ACP registry and the built-in entries for Kiro and Amp. Install asks for the global or the project settings.
10. Install uses the distribution of the registry entry: `npx` and `uvx` store the command; `binary` downloads the archive for the platform, checks the SHA-256 when the entry has one, and unpacks it under `~/.pi/agent/acp/agents/<id>/<version>/`.
11. Log in shows the `authMethods` of the agent. A terminal method (`type: "terminal"` or `_meta["terminal-auth"]`) runs in the terminal while the TUI is stopped. Other methods go to `authenticate`.
12. Tests use a fake ACP agent and a local mock HTTP server. They cover initialize, prompt streaming, tool call updates, permission requests, fs read and write, cancel, registry fetch, install, and criteria 13 to 17. No test uses the network, a real agent, or a key.
13. `session/new` and `session/load` get the enabled, not hidden servers of `mcp.json`, plus `.pi/mcp.json` in a trusted project. A stdio server always goes to the agent, with a bare command resolved on PATH. An `http` or `sse` server with its own headers goes to the agent only when `agentCapabilities.mcpCapabilities` has that transport. An HTTP server whose login pi holds (OAuth, or `auth.provider`) goes through `pi-mcp`, a loopback MCP server with a bearer token: the agent lists its tools (in pages of 64 with `nextCursor`), and pi calls them over its own MCP connection. `pi-mcp` needs `mcpCapabilities.http`. When the agent cannot take a server, the reply starts with a line that names it.
14. `available_commands_update` gives the slash commands of the agent. pi keeps them per provider in `state.json`, and the editor completes them while an ACP model is selected. `/<name> <input>` goes to the agent as prompt text. A name that a pi command uses completes as `/acp:<name>`, and the provider sends `/<name>`.
15. The `size` of `usage_update` is the context window of the pi model, per provider and model ID. Before an agent sends one, the window and `maxTokens` are 0, which means unknown: threshold compaction does not run, and the footer and `/context` show `?` or `unknown`. The model input has images unless the agent reported `promptCapabilities.image: false`.
16. A request that is not a turn of a live pi session, such as a compaction or branch summary, runs in a new ACP process with a new session. That session gets the system prompt of the request, no MCP servers, and no `session/load`. pi rejects its file writes and terminals, cancels its permission requests, and records nothing in `state.json`. After the reply or a cancel, pi stops the process group of the agent, which includes the processes it started, and waits for the agent to exit. pi stops the group the same way when it drops any agent process.
17. `/undo` covers ACP turns. Before a turn, pi keeps the files of the working directory that `.gitignore` does not exclude (hidden files included, `.git` not). It keeps up to 4 MiB a file and 256 MiB in all. After the turn, pi compares the content and records new, changed, and deleted files. `fs/write_text_file` and the files named by `edit`, `delete`, and `move` tool calls are recorded as reported changes. Other differences are marked as not reported, and `/undo` lists them and asks before restoring them.

## Decision: a pi-ai provider

pi runs an ACP agent as a pi-ai provider with the API `acp` (`crates/pi-ai/src/providers/acp/`). pi does not add a second session backend.

Reasons:

- The agent runs its own tools, so the pi agent loop only streams. The provider is the smallest surface that does this, and `pi-ai::providers::antigravity_acp` already is an ACP client provider.
- The model selector, session files, Escape, retries, and the `MessageBuilder` rule work without changes.
- A session backend would replace `AgentSession` in the interactive, print, and RPC modes.

Consequences:

- pi does not send its system prompt or its tools to the agent. The agent uses its own.
- The tool calls of the agent are text in the assistant message. A `ToolCall` block would make the pi loop run a pi tool with that name.
- The UI work (permission dialogs, the plan widget, the session map for `session/load`) goes through the `AcpHost` hook. `pi-coding-agent` sets it (`src/acp_client/host.rs`).

## Research

All sources read on 2026-10-30.

### Protocol

Source: [schema/v1/schema.json](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json), schema release 1.28.0.

- The protocol version is `1`. The draft `schema/v2` removes the `fs` and `terminal` methods.
- Auth methods: an agent method `{id, name, description}`, or `type: "terminal"` with `args` and `env`. The client runs the agent command with these arguments in a terminal and does not send `authenticate` for it. Agents show terminal methods only when the client sends `clientCapabilities.auth.terminal: true`. amp-acp uses the older `_meta["terminal-auth"] = {command, args, label}`.
- The unstable `models` field and `session/set_model` were removed on 2026-06-01 (commit `822ef53cd`, #1325). Models come in `configOptions` with `category: "model"`. pi still reads `models.availableModels` and sends `session/set_model` to agents that do not send `configOptions`.
- `configOptions` categories: `mode`, `model`, `model_config`, `thought_level`, `_custom`. `session/set_config_option` takes `{sessionId, configId, value}`.
- Stop reasons: `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled`.
- Read on 2026-10-31 from the same schema: `McpServer` is stdio (`name`, `command` as an absolute path, `args`, `env` as `{name, value}` pairs), which every agent must support, or `type: "http"` and `type: "sse"` (`name`, `url`, `headers`), which an agent takes only with `agentCapabilities.mcpCapabilities.http` or `.sse`.
- `AvailableCommandsUpdate` has `availableCommands` of `{name, description, input?: {hint}}`. A client runs a command by sending `/<name> <input>` as prompt text.
- `UsageUpdate` has `used` (tokens in context) and `size` (the context window), both required, and an optional cumulative `cost`.
- `Diff` has `path`, `newText`, and `oldText` (`null` for a new file). `ToolCallLocation` has `path` and an optional `line`. Both are hints for the client: they do not have to name every file a tool changes.

### Registry

Source: [agentclientprotocol/registry](https://github.com/agentclientprotocol/registry), `FORMAT.md` and `AUTHENTICATION.md`.

- Index: `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`, shape `{version, agents[], extensions[]}`. It listed 43 agents.
- Entry: `id`, `name`, `version`, `description`, `repository`, `website`, `authors`, `license`, `icon`, and `distribution` with one or more of:
  - `npx: {package, args, env}`, run as `npx <package> <args>`;
  - `uvx: {package, args}`, run as `uvx <package> <args>`;
  - `binary: {<platform>: {archive, sha256, cmd, args, env}}`. The platforms are `darwin-aarch64`, `darwin-x86_64`, `linux-aarch64`, `linux-x86_64`, `windows-aarch64`, `windows-x86_64`. The archive is `.zip`, `.tar.gz`, `.tgz`, `.tar.bz2`, `.tbz2`, or a raw binary. `cmd` is relative to the unpacked archive.
- Agents in the registry support the agent or the terminal auth method.

### Kiro

Sources: [Kiro CLI V3](https://kiro.dev/docs/cli/v3/), [ACP migration for V3](https://kiro.dev/docs/cli/v3/acp-migration/), [Kiro CLI ACP](https://kiro.dev/docs/cli/acp/), [authentication](https://kiro.dev/docs/getting-started/authentication/), registry PR [#561](https://github.com/agentclientprotocol/registry/pull/561).

- "Kiro V3" is Kiro CLI V3, which uses the agent harness of Kiro IDE. It runs next to Kiro CLI 2.x.
- Install: `curl -fsSL https://cli.kiro.dev/install | bash`. The program is `kiro-cli`.
- ACP: `kiro-cli acp --agent-engine=v3` (V3) or `kiro-cli acp` (2.x). V3 rejects `--agent`, `--model`, `--effort`, `--trust-all-tools`, and `--trust-tools`.
- Login: `kiro-cli login` (browser or device code). `KIRO_API_KEY` works in non-interactive mode.
- V3 sends models and the effort level in `configOptions` (`model`, `effortLevel`) and modes with `session/set_mode`. 2.x supports `session/set_model`.
- The registry does not list Kiro: PR #561 is open. pi has a built-in entry.

### Amp

Sources: [amp-acp](https://github.com/tao12345666333/amp-acp) `src/server.ts`, the registry entry `amp-acp`, and the [Amp manual](https://ampcode.com/manual).

- Adapter: npm `amp-acp` 0.10.0 (community), run with `npx -y amp-acp`. The registry entry `amp-acp` ships binaries. `AMP_CLI_PATH` selects the `amp` program.
- The Amp CLI has no ACP mode of its own.
- Login: `amp login`, or `AMP_API_KEY`. The adapter offers the auth method `setup` with `_meta["terminal-auth"]` (`--setup`).
- Amp sends `configOptions`: `execution-environment` and `permission` (category `mode`) and `amp-mode` (category `model`, values `low`, `medium`, `high`, `ultra`).

## Use an ACP agent

1. Run `/acp`. The **Registry** tab lists the agents of the registry, plus Kiro CLI (V3) and Amp.
2. Select an agent and press Enter, then choose the global settings (every project) or the project settings (this project only). A project entry replaces a global entry with the same ID.
3. In the **Installed** tab, select the agent and choose **Log in** if the agent needs a login. For Kiro without `kiro-cli` on PATH or in `~/.local/bin`, Install offers to run the official installer (`curl -fsSL https://cli.kiro.dev/install | bash`) in the terminal after you confirm. Then log in with `kiro-cli login` or the **Log in** action. Kiro CLI exits before ACP starts when it has no login, so **Log in** runs `kiro-cli login` in the terminal. The installer and an authenticated Kiro session were not run in the tests.
4. Select the model `<provider>/default` with `/model`, or choose **Set as default**. pi opens that provider's session, and `/model` then lists its advertised models without a prompt. A selected ACP provider also opens at startup. Other providers are not started just to populate the list.
5. Run `/acp mode` to change an advertised permission mode or another agent setting. Run `/thinking` to choose a supported thinking level. These controls open the session when needed; a failed change leaves the local selection unchanged.

The agent edits files and runs commands with its own tools. pi shows its permission requests as dialogs. In print mode (`pi -p`), pi rejects the requests.

## Settings

`acpAgents` is an array in the global or the project `settings.json`. A project entry replaces a global entry with the same `id`.

```json
{
  "acpAgents": [
    {
      "id": "kiro",
      "name": "Kiro CLI (V3)",
      "command": "kiro-cli",
      "args": ["acp", "--agent-engine=v3"],
      "env": {},
      "enabled": true,
      "source": "builtin"
    }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `id` | The agent ID. The provider ID is `<id>-acp`, or `<id>` when the ID ends in `-acp`. |
| `name` | The name in `/model`, `/acp`, and dialogs. |
| `command`, `args`, `env` | The command that starts the agent in ACP mode over stdio. |
| `enabled` | `false` removes the provider from `/model` and keeps the entry. |
| `loginCommand` | The login program for agents that exit before ACP starts without a login, for example `["kiro-cli", "login"]`. **Log in** runs it when the agent does not start. |
| `version`, `description`, `source`, `installDir` | Set by the install. `installDir` is removed with the entry. |

The `acpRegistryUrl` setting replaces the registry URL, for example with a mirror. `PI_ACP_REGISTRY_URL` replaces both.

pi keeps these files in `~/.pi/agent/acp/`:

- `agents/<id>/<version>/`: unpacked binary distributions.
- `state.json`: the ACP session ID of each pi session (for `session/load`), and per agent the models, slash commands, context windows, and image support it advertised.

## Limits

- `/undo` cannot tell who made a change during an ACP turn. A change that you or another program make in the working directory during the turn is a candidate too. Only changes the agent reported are attributed to it.
- pi tracks only the working directory. pi's `fs` and `terminal` methods refuse paths outside it, but the agent's own tools are not confined. Changes outside the working directory are not tracked or undone.
- A file that `.gitignore` excludes is tracked only when the agent names it in `fs/write_text_file` or in the locations or diff of an `edit`, `delete`, or `move` tool call before the change.
- A file larger than 4 MiB, or a file beyond the 256 MiB total, is found by its size and time only. `/undo` lists it as not restorable, with the reason.
- `/undo` restores a file only while it holds what the turn left. It leaves a file that changed after the turn, and it does not restore a deleted file that came back.
- The stdio `cwd` of an `mcp.json` server is not sent: ACP has no field for it.
- pi runs the tool calls that come through `pi-mcp` without its own permission dialog. The agent asks for permission for its MCP tools in its own way.
- `pi-mcp` lists the tools that are connected when the agent asks. pi updates the list at each turn, but an agent that lists tools only once at the start of a session does not see servers that connect later.
- Process group cleanup is for Unix. On Windows, pi stops only the agent process.
- pi compacts its own transcript. The agent keeps its context and compacts it itself.
