# Deferred context

Created: 2026-11-04. Last updated: 2026-11-04.

pi-rust can keep skills and extension context entries, such as subagent configs, out of the system prompt. The model finds them with the built-in `tool_search` tool, as it finds deferred tools. Upstream pi has no such feature: it lists every skill in the system prompt.

## Settings

The `contextExposure` setting maps a kind of context to `"direct"` or `"deferred"`. A kind that is not in the map is `direct`, so the default behavior does not change.

```json
{
  "contextExposure": {
    "skill": "deferred",
    "agent": "deferred"
  }
}
```

- `skill` is the kind of all skills.
- Extensions choose the kind of their entries. The subagents extension should use `agent`.
- `direct` lists the item in the system prompt.
- `deferred` leaves the item out of the system prompt. The `deferred_context` section counts the deferred items of each kind and tells the model to use `tool_search`.

An item can set its own exposure, and that value wins over the setting of its kind. For a skill, add `exposure: deferred` or `exposure: direct` to the `SKILL.md` frontmatter. Another value gives a warning, and pi then uses the setting.

Items are deferred only while `tool_search` is declared to the model. When some item is deferred, pi activates the built-in `tool_search` before each prompt, as it does for MCP servers with `deferred` exposure. If `tool_search` is off (`-builtin:tool-search` in `extensions`) or hidden, pi lists the items in the system prompt again, so they stay reachable. Skills with `disable-model-invocation: true` are never listed and never found.

## What tool_search returns

`tool_search` ranks the deferred tools and the deferred context entries together with BM25, under one `limit`. The search text of an entry is its name, kind, description, and usage. A query that names an entry ranks it first.

A tool match loads the tool, as before. A context match loads nothing. The result text tells the model how to use the entry:

```text
Found 1 context entry. Nothing was loaded; use them as described:
- skill deploy: Deploy the app to production.
  Read /work/.pi/skills/deploy/SKILL.md to load its instructions. Resolve relative paths in it against /work/.pi/skills/deploy.
```

When the search finds tools and entries, the tool lines come first, in the format of a tools-only search. The entry lines follow after a blank line. The result `details` are `{ loaded, context }`, where `context` is `[{ kind, name }]`. When no entry matches, the text and `details` are the same as before.

## Extension API: context entries

An extension adds context entries with the `contextEntries` list of its `resources_discover` result. The list goes next to `skillPaths` and `promptPaths`. JavaScript and native extensions use the same result.

```js
pi.on("resources_discover", () => ({
  contextEntries: [
    {
      kind: "agent",
      name: "reviewer",
      description: "Reviews a diff and lists bugs.",
      usage: 'Call the subagent tool with agent "reviewer".',
    },
  ],
}));
```

| Field | Use |
| --- | --- |
| `kind` | Required. Lowercase letters, digits, `_`, and `-`, starting with a letter. The `contextExposure` setting uses it as the key. |
| `name` | Required. Unique within the kind. When an entry has the same kind and name as an earlier entry, pi drops it. |
| `description` | Required. The first line shows in `tool_search` results. |
| `usage` | Optional. Tells the model how to load or use the entry, for example which tool to call. |
| `path` | Optional. A file that the model reads to load the entry. pi adds "Read `<path>` to load it." to the usage. |
| `exposure` | Optional. `"direct"` or `"deferred"`. It wins over `contextExposure`. Set it only when the user chose it for this item, for example in the item's own frontmatter. |

pi drops an entry that has an invalid kind, no name, no description, or an invalid `exposure`. Each discovery (at startup and after `/reload`) replaces the entries of the previous one.

pi lists direct entries in the `context_entries` system prompt section, in an `<available_context>` block that has the kind, name, description, usage, and location of each entry. pi counts deferred entries in `deferred_context`. `before_agent_start` handlers do not see the entries in `systemPromptOptions`. Their edits do not change the entries.

### Subagents extension

The subagents extension in pi-rust-extensions adds the `subagents` section in `before_agent_start` at this time. To support deferral, it can do these steps:

1. Return one entry for each agent from `resources_discover`, with `kind: "agent"`, the agent's `name` and `description`, and a `usage` such as `Call the subagent tool with agent "<name>".`
2. Read an `exposure` frontmatter key from agent files and pass it as `exposure`.
3. Stop adding the `subagents` section, or add it only for agents that pi lists.
