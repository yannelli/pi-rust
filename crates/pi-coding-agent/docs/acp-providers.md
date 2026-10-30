# ACP agents as providers

Created: 2026-10-30. Last updated: 2026-10-30.

pi can run an external [Agent Client Protocol](https://agentclientprotocol.com) (ACP) agent, such as Kiro CLI or Amp, and show its work in the pi transcript. This page has the acceptance criteria, the design decision, the research with sources, and the user guide. For pi as an ACP server (`pi --mode acp`), see [acp.md](acp.md).

## Acceptance criteria

1. Each enabled entry of `acpAgents` is a provider `<id>-acp` (or `<id>` when the ID ends in `-acp`) in `/model`, with the API `acp`. Its models are `default` plus the models that the agent advertised in its last session.
2. A request starts the agent command over stdio in the session working directory, sends `initialize` (protocol 1, `fs` and `terminal` client capabilities, `auth.terminal`), then `session/new`. A later request of the same pi session reuses the ACP session. After a restart of pi, `session/load` resumes the ACP session when the agent advertises `loadSession`. Otherwise pi sends the earlier conversation as a context resource.
3. `session/prompt` gets the user text, images (when `promptCapabilities.image`), and a `resource_link` for each file that the user message names with `@`.
4. `session/update` goes into one assistant message: message chunks as text, thought chunks as thinking, tool calls as text lines with the kind, title, and status, with diffs as fenced `diff` blocks and terminal output when the call ends, plans as a checklist, and `usage_update` as context use. Every stream ends with `Done` or `Error` through `MessageBuilder`.
5. `session/request_permission` shows a dialog with the options of the agent (allow once, allow always, reject once, reject always). Without a UI, pi rejects. A cancel answers open requests with `cancelled`.
6. `fs/read_text_file`, `fs/write_text_file`, and `terminal/create`, `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, `terminal/release` work for paths inside the session working directory. Paths outside it fail.
7. Escape sends `session/cancel`. The stream ends with stop reason `aborted`.
8. Modes (`modes` or a `configOptions` selector of category `mode`) are selectable with `/acp mode`. Models (`configOptions` category `model`, or `models.availableModels` of older agents) are selectable in `/model`.
9. `/acp` opens a browser with two tabs. **Installed** lists the agents in `acpAgents` (global and project scope): remove, enable or disable, set as default, and log in. **Registry** lists the ACP registry and the built-in entries for Kiro and Amp: install.
10. Install uses the distribution of the registry entry: `npx` and `uvx` store the command; `binary` downloads the archive for the platform, checks the SHA-256 when the entry has one, and unpacks it under `~/.pi/agent/acp/agents/<id>/<version>/`.
11. Log in shows the `authMethods` of the agent. A terminal method (`type: "terminal"` or `_meta["terminal-auth"]`) runs in the terminal while the TUI is stopped. Other methods go to `authenticate`.
12. Tests use a fake ACP agent and a local mock HTTP server. They cover initialize, prompt streaming, tool call updates, permission requests, fs read and write, cancel, registry fetch, and install. No test uses the network, a real agent, or a key.

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
2. Select an agent and press Enter to install it. pi adds it to the global `~/.pi/agent/settings.json`. To use it in one project only, move the entry to `.pi/settings.json`.
3. In the **Installed** tab, select the agent and choose **Log in** if the agent needs a login. For Kiro, install Kiro CLI first (`curl -fsSL https://cli.kiro.dev/install | bash`), then log in with `kiro-cli login` or the **Log in** action. Kiro CLI exits before ACP starts when it has no login, so **Log in** runs `kiro-cli login` in the terminal.
4. Select the model `<provider>/default` with `/model`, or choose **Set as default**. After the first prompt, `/model` also lists the models that the agent advertised.
5. Run `/acp mode` to change the mode or another setting of the agent session.

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

`PI_ACP_REGISTRY_URL` replaces the registry URL.

pi keeps these files in `~/.pi/agent/acp/`:

- `agents/<id>/<version>/`: unpacked binary distributions.
- `state.json`: the ACP session ID of each pi session (for `session/load`) and the models each agent advertised.

## Not done yet

- pi does not send its MCP servers to the agent in `session/new`.
- `available_commands_update` (the slash commands of the agent) is not shown.
- The context window of the agent models is a fixed 1,000,000 tokens, so pi does not compact the transcript. The agent manages its own context.
- Agent file writes do not go into the pi `/undo` checkpoints.
- `/compact` sends its summary request to the agent session like any prompt.
