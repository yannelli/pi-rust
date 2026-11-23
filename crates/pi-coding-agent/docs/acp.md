# ACP mode

`pi --mode acp` runs pi as an [Agent Client Protocol](https://agentclientprotocol.com) (ACP) server over stdio. Editors such as Zed can then use pi as an external agent. Upstream pi needs the separate pi-acp adapter for this.

## Zed

Add this to the Zed `settings.json`:

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

## Sessions

Each editor session runs a pi session in its project directory. pi saves it in `~/.pi/agent/sessions/`, as it does all sessions. Thus the editor can list, resume, and delete these sessions. `pi -r` can resume them too.

- One connection can run several sessions, as Zed does for the threads of a project. A new or loaded session leaves the others open and running. pi-acp and earlier versions of pi closed them, which aborted their runs.
- When more than 8 sessions of a connection are open, pi closes the idle ones the client used least recently. A session with a prompt or a run stays open.
- `session/close` (`sessionCapabilities.close`) cancels the session's prompt and frees the session. Zed sends it when it closes a thread. A later request reopens a closed session from its file, without client MCP servers until `session/load`.
- `~/.pi/pi-acp/session-map.json` maps ACP session ids to session files. All pi processes in ACP mode share it. pi replaces it through a temporary file, so no process reads a half-written map.
- pi writes a session file when the session gets its first message. `session/load` of a session that never got a prompt starts the file with the session id and the `cwd` of the request, so the session keeps its id.

## Prompts

- A prompt that arrives while another prompt of the session runs waits in a queue. pi sends `Queued message (position n).` and starts it when the running turn ends.
- pi can run on its own, for example when an extension sends a background result with `triggerTurn` or `pi.sendUserMessage`. A prompt that arrives then joins that run as a steering message. Its turn ends when the run ends.
- A turn ends with the run that its prompt started or joined, also when the event that ends an earlier run arrives late.
- `session/cancel` stops the run and drops the prompts that pi queued for it. A canceled prompt that pi did not take yet, for example while extension `input` handlers run, sends no model request.
- When pi rejects a prompt, `session/prompt` fails with a JSON-RPC error (code `-32603`) whose message is pi's message. Missing credentials give `AUTH_REQUIRED` (`-32000`), as before. Earlier versions ended such a turn with `end_turn` and no text.
- When the turn's run ends with a failed model request, `session/prompt` fails the same way, with the provider's error message. Paseo marks such a turn failed and shows the message. Zed shows it in an error callout. Earlier versions ended the turn with `end_turn` and no text.
- Before the turn fails, pi retries transient errors, as set in the `retry` settings. Meanwhile, it shows `Retrying (attempt n/max, waiting s)...`. Only the last request of the run counts: a retry that succeeds ends the turn normally.

## Editor features

These features of the editor work:

- The model and thinking selectors.
- The **Fast mode** selector (pi-rust extension, choices **Fast off** and **Fast on**). It shows only for models that have a fast mode. See [Fast mode](providers.md#fast-mode-pi-rust-extension).
- Tool call diffs and terminals. See [Bash calls](#bash-calls).
- Context usage.
- Slash commands:
  - Prompt templates and `/skill:name`
  - `/compact` and `/autocompact`
  - `/export`, `/session`, and `/name`
  - `/steering` and `/follow-up`
  - `/changelog`

  pi sends the list after the extensions are bound, so it has the skills and prompt templates that extensions add. Skills kept out of the system prompt, such as deferred or user-only skills, are in the list too.
- The pi-rust commands `/usage`, `/context`, `/extensions`, `/fast`, and `/reload`. They run in pi without a model call and answer with an agent message. [RPC mode](rpc.md#built-in-commands) describes each one. After `/reload` or an extension change, pi sends `available_commands_update` again.
- `/mcp` while MCP runs for the session. Its notices are agent messages with `_meta.piAcp.notify`.

The text and thought chunks of each assistant message share one `messageId`. The notices of pi are agent messages with a `messageId` of their own, so clients show them apart from the model's reply. Examples are `Retrying (attempt n/max, waiting s)...`, the compaction notices, the queue notices, and the startup information.

When the model, the thinking level, or fast mode changes inside the session, for example through `/fast` or an extension, pi sends `config_option_update` with the new options.

Each model in `models.availableModels` and in the choices of the Model option has `_meta` with its own thinking levels:

- `supportsReasoningEffort`.
- `reasoningEfforts`: each level has `id` and `label`. The level that pi would use with that model also has `default`.
- `reasoningEffort`: the level that pi would use with that model.

Paseo uses them to show the thinking choices of each model. The Thinking option shows the levels of the current model.

Model labels use the display names of the providers. Catalog variants with a `fast` segment in their ID have a ⚡ label. ACP has no standard field for a model icon.

## Bash calls

- A bash call is an `execute` tool call with terminal content and the `_meta` fields `terminal_info`, `terminal_output`, and `terminal_exit`. Zed shows them as a terminal. pi-acp sends the same fields.
- Each bash call also has `rawInput` with the arguments, such as `{"command": "ls"}`, and `rawOutput` with `output` and, when the command ended, `exitCode`. The exit code is the one of the command, also in `terminal_exit`.
- A client that does not send `clientCapabilities._meta.terminal_output: true`, such as Paseo, also gets the output as text content next to the terminal. Zed sends this capability, so it gets no copy of the output.
- `session/load` replays bash calls in the same form, with the command from the stored tool call.

## Tool approval

The Mode selector controls tool approval. It is independent of the Thinking selector.

| Mode | Effect |
| --- | --- |
| Auto (the default) | Allow the tools that have a read-only mark. Ask before other tools. |
| Ask | Ask before each tool call. |
| Bypass | Skip ACP tool approvals. Extension dialogs still apply. |

- An approval failure, a rejected request, or a cancellation stops the tool call.
- The modes control approval only. They do not isolate tools from the system. Auto trusts the read-only annotations of the tool authors, and read-only tools can read outside the project.
- If each call needs a review, use Ask.
- New connections start in Auto. pi does not save Bypass.
- A permission request for an MCP server tool has `toolCall._meta.piAcp.mcp` with the server name and the server's own tool name, as `{"server": "...", "tool": "..."}`. The tool title is the pi tool name, which can differ. A call that another tool made, for example from `codemode`, has `_meta.piAcp.parentToolCallId`.
- Other tools, resource tools, and tools of other sources with the same name have no `mcp` field. The request contains no argument values in `_meta`.
- The `initialize` response has `_meta.piAcp.exactMcpPreapproval: true` when pi sends these fields and accepts `clientMcpExposure`.

## Extension dialogs

- An extension `select` or `confirm` becomes a `session/request_permission` request. The tool call title is the dialog title, followed by the message of a `confirm`. `rawInput` has the fields of the dialog.
- The options of a `select` are its choices. A `confirm` has the options **Yes** and **No**.
- All options of a dialog have the kind `allow_once`. Clients that approve tool calls on their own, such as Paseo with **Auto Accept**, take a request whose options share an allow kind for a choice and leave it to the user. Before, **Yes** had `allow_once` and **No** `reject_once`, so Auto Accept answered **Yes**. A `select` with one option is still answered by such clients.
- `input` and `editor` dialogs are canceled with a note. `notify` becomes an agent message with `_meta.piAcp.notify.level`.

## MCP servers from the client

- pi connects the stdio and `http` `mcpServers` of `session/new` and `session/load` for that session, next to the servers of the pi `mcp.json` files. Project trust still applies to `.pi/mcp.json` only. The `initialize` response has `mcpCapabilities.http: true`.
- For a stdio server, `command` must be an absolute path. pi runs it with `args` as separate arguments and without a shell, and uses `command`, `args`, and `env` exactly as sent: no `${VAR}`, `!command`, or `~` expansion.
- For an `http` server, pi uses the Streamable HTTP transport with `url` and `headers` exactly as sent, also without expansion. pi does not log in to these servers with OAuth. A server that needs credentials must get them from the headers.
- pi does not save these servers or their values. It keeps no server stderr, writes no server messages to `mcp.log`, keeps no tool list of them in `mcp-cache/`, and names only the server in errors, without the URL. A server can still return its own values in tool results.
- pi rejects `sse` servers, because `mcpCapabilities.sse` is false: pi has no transport for the legacy SSE protocol. It also rejects a list that has one of these:
  - A relative command.
  - A URL that is not `http` or `https`.
  - A header name or value that HTTP does not allow.
  - A NUL byte.
  - A repeated env or header name.
  - Two servers whose tool names would be the same.

  A rejected list starts nothing and leaves the open session as it was.
- When a client server has the tool names of a server from `mcp.json` or from an extension, the client server is used for the session and `/mcp` shows a notice. This is a decision of pi; the protocol does not require it.
- To give the model the client servers' tools directly, without `codemode`, send `"_meta": {"piAcp": {"clientMcpExposure": "direct"}}` in `session/new` or `session/load`. `"direct"` is the only value. pi rejects other values before it starts or closes anything. Servers from `mcp.json` keep their own exposure, and pi does not save this setting.
- A session that pi reopens without `session/load`, for example for a prompt after the client closed it, has no client servers. Send `session/load` with the list again.

Paseo 0.14 and later send an `http` server named `paseo` with an `Authorization` header to each agent. pi connects it like the other client servers, so `paseoTools` can stay on. Earlier versions of pi rejected it, and `session/new` and `session/load` failed.

## Known limits

- Paseo shows the error `cannot preapprove exact MCP tools for unattended execution` before ACP starts. The cause is a provider policy in the host. A change to the pi mode does not remove the error. The Paseo ACP adapter needs support for exact-tool preapproval. Do not use Bypass in place of an exact-tool allowlist.

## Options

- To hide the startup information block, set `quietStartup: true` in the pi settings.
- To accept embedded file contents in prompts, set `PI_ACP_ENABLE_EMBEDDED_CONTEXT=true`.

If no models are configured, the editor offers to start pi in a terminal, where you can log in.

## Rust clients

When implementing `pi_coding_agent::acp::AcpClient`, import `SessionNotification`, `RequestPermissionRequest`,
`RequestPermissionResponse`, and `Error` from `pi_coding_agent::acp::schema`. These replace the corresponding
`agent_client_protocol_schema` imports. Read the JSON-RPC error code directly from the `i32` field `error.code`.
Clients that connect over stdio can use their existing ACP SDK.

ACP mode is a port of [pi-acp](https://github.com/svkozak/pi-acp) by Sergii Kozak (MIT license).
