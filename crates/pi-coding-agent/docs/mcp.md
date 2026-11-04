# MCP server lifecycle (pi-rust extension)

By default pi connects every enabled MCP server of `mcp.json` in the background when a session starts. A server with `"lifecycle": "lazy"` connects only when a session uses it. Upstream pi has no `lifecycle` field.

## Configuration

```json
{
  "lifecycle": "lazy",
  "mcpServers": {
    "github": { "command": "github-mcp", "exposure": "deferred" },
    "docs": { "url": "https://docs.example/mcp", "lifecycle": "eager" }
  }
}
```

- `lifecycle` on a server is `"eager"` (connect when the session starts) or `"lazy"` (connect on first use). pi rejects other values with `server "<name>": lifecycle must be "eager" or "lazy"`.
- The top-level `lifecycle` is the default for servers that do not set their own. Without it, the default is `"eager"`. When the global and the trusted project `mcp.json` both set it, the project value applies.
- A project `.pi/mcp.json` can override `lifecycle` of a global server, like `enabled`, `exposure`, and `toolExposure`.
- `/mcp` shows the lifecycle in the server menu: **Connect: at startup** or **Connect on first use**. Choose the item to switch. pi saves the change to the `mcp.json` that defines the server, or to its project override. pi writes `"lifecycle"` also for `"eager"`, because the top-level default can be `"lazy"`.
- `pi mcp add <server> --lifecycle lazy ...` adds a lazy server.
- Servers that extensions register and servers that an ACP client sends always connect when the session starts.

## Tool cache

Each time a server from `mcp.json` lists its tools, pi saves the list to `<agent dir>/mcp-cache/<server>.json`. `pi mcp list` also saves it. The file holds each tool as the server sent it (name, title, description, input and output schemas, and annotations), the server instructions, and a SHA-256 fingerprint of the config file path and the connection settings (`type`, `command`, `args`, `env`, `cwd`, `url`, `headers`, `oauth`, and `auth`). A change to one of these settings makes the saved list invalid. A change to `exposure`, `toolExposure`, `description`, `timeout`, `enabled`, or `lifecycle` keeps it.

pi does not save the tools of extension servers and ACP client servers.

## When a lazy server connects

At the start of a session, a lazy server with a valid tool cache registers the saved tools with its configured exposure. `tool_search`, codemode `searchTools()`, `describeNamespace()`, the `mcp_servers` prompt section, and `direct` declarations see these tools, and pi does not connect. `/mcp` shows the server as `idle, connects on first use`.

A lazy server connects in these cases:

- A call of one of its tools, directly or from a codemode script. pi connects the server, waits for it up to its `timeout` (default 60 seconds), and runs the call on the live server. The live tool list replaces the saved one and updates the cache. If the server does not connect, the call fails with a tool error: `MCP server "<name>" failed to connect: <reason>`, `MCP server "<name>" did not connect within <n>s.`, or the sign-in message of an OAuth server.
- `tool_search`, or a codemode script that uses `searchTools`, `describeNamespace`, `describeTool`, or `ALL_TOOLS` or names the server's namespace, when the server has no valid tool cache. pi connects these servers first and then runs the tool.
- The first prompt, when the server has `direct` tools and no valid tool cache. A lazy server with a valid cache does not connect for the first prompt.
- `pi.callMcpTool` and the native method `mcp.callTool`.
- **Connect** in the `/mcp` server menu, or `/mcp reconnect <server>`.
- A switch to **Connect: at startup** in `/mcp`.

The resource tools (`list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource`) reach a lazy server only after it connected. pi does not disconnect an idle server.
