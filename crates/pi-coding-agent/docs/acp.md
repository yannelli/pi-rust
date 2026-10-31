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

## Known limits

- Paseo shows the error `cannot preapprove exact MCP tools for unattended execution` before ACP starts. The cause is a provider policy in the host. A change to the pi mode does not remove the error. The Paseo ACP adapter needs support for exact-tool preapproval. Do not use Bypass in place of an exact-tool allowlist.
- pi does not load the `mcpServers` that the client supplies. Configure servers in the pi `mcp.json` files.

## Options

- To hide the startup information block, set `quietStartup: true` in the pi settings.
- To accept embedded file contents in prompts, set `PI_ACP_ENABLE_EMBEDDED_CONTEXT=true`.

If no models are configured, the editor offers to start pi in a terminal, where you can log in.

ACP mode is a port of [pi-acp](https://github.com/svkozak/pi-acp) by Sergii Kozak (MIT license).
