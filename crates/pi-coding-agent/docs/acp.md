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

## Editor features

These features of the editor work:

- The model and thinking selectors.
- The **Speed** selector (pi-rust extension). It shows only for models that have a fast mode. See [Fast mode](providers.md#fast-mode-pi-rust-extension).
- Tool call diffs and terminals.
- Context usage.
- Slash commands: prompt templates, `/skill:name`, `/compact`, `/autocompact`, `/export`, `/session`, `/name`, `/steering`, `/follow-up`, and `/changelog`.

Model labels use the display names of the providers. Catalog variants with a `fast` segment in their ID have a ⚡ label. ACP has no standard field for a model icon.

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
- A permission request for an MCP server tool has `toolCall._meta.piAcp.mcp` with the server name and the server's own tool name, as `{"server": "...", "tool": "..."}`. The tool title is the pi tool name, which can differ. A call that another tool made, for example from `codemode`, has `_meta.piAcp.parentToolCallId`. Other tools, resource tools, and tools of other sources with the same name have no `mcp` field. The request contains no argument values in `_meta`. The `initialize` response has `_meta.piAcp.exactMcpPreapproval: true` when pi sends these fields and accepts `clientMcpExposure`.

## MCP servers from the client

- pi connects the stdio `mcpServers` of `session/new` and `session/load` for that session, next to the servers of the pi `mcp.json` files. Project trust still applies to `.pi/mcp.json` only.
- `command` must be an absolute path. pi runs it with `args` as separate arguments and without a shell, and uses `command`, `args`, and `env` exactly as sent: no `${VAR}`, `!command`, or `~` expansion.
- pi does not save these servers or their values. It keeps no server stderr, writes no server messages to `mcp.log`, and names only the server in errors. A server can still return its own values in tool results.
- pi rejects `http` and `sse` servers, because `mcpCapabilities.http` and `.sse` are false. It also rejects a list with a relative command, a NUL byte, a repeated env name, or two servers whose tool names would be the same. A rejected list starts nothing and leaves the open session as it was.
- When a client server has the tool names of a server from `mcp.json` or from an extension, the client server is used for the session and `/mcp` shows a notice. This is a decision of pi; the protocol does not require it.
- To give the model the client servers' tools directly, without `codemode`, send `"_meta": {"piAcp": {"clientMcpExposure": "direct"}}` in `session/new` or `session/load`. `"direct"` is the only value. pi rejects other values before it starts or closes anything. Servers from `mcp.json` keep their own exposure, and pi does not save this setting.
- A session that pi reopens without `session/load`, for example for a prompt after the client closed it, has no client servers. Send `session/load` with the list again.

Paseo 0.14 adds an HTTP `paseo` server by default. For a custom Pi ACP provider, set `providers.<provider-id>.paseoTools.enabled` to `false` and send only stdio servers. Pi rejects unsupported transports instead of dropping them.

## Known limits

- Paseo shows the error `cannot preapprove exact MCP tools for unattended execution` before ACP starts. The cause is a provider policy in the host. A change to the pi mode does not remove the error. The Paseo ACP adapter needs support for exact-tool preapproval. Do not use Bypass in place of an exact-tool allowlist.

## Options

- To hide the startup information block, set `quietStartup: true` in the pi settings.
- To accept embedded file contents in prompts, set `PI_ACP_ENABLE_EMBEDDED_CONTEXT=true`.

If no models are configured, the editor offers to start pi in a terminal, where you can log in.

ACP mode is a port of [pi-acp](https://github.com/svkozak/pi-acp) by Sergii Kozak (MIT license).
