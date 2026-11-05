# Native extension protocol

Created 2026-10-29. Last updated 2026-11-02.

A native extension is a program that pi starts as a child process. pi and the child exchange JSON-RPC 2.0 messages. The built-in bridge (`crates/pi-coding-agent/src/extensions/js/native*.js`) runs in the extension runtime and maps each message onto the `pi` and `ctx` objects of the JavaScript extension API. The Rust SDK is `crates/pi-ext-sdk` in `yannelli/pi-rust-extensions`.

## Manifest

A package declares native extensions in `package.json`:

```json
{ "name": "@pi-rust-extensions/todo", "pi": { "native": [{ "command": "bin/pi-rust-ext", "args": ["todo"], "env": {} }] } }
```

- `command`: an absolute path, or a path relative to the package directory. On Windows pi adds `.exe` when the name has no extension.
- `args` and `env` are optional. pi adds `env` to its own environment.
- Each entry loads as one extension. Its extension path is `native:<package name>/<args[0] or file name>`.
- `--extension <file>` and the settings `extensions` list also accept an executable file that is not `.ts` or `.js`. Its path is `native:<file name>`.

## Transport

- One JSON object per line (UTF-8, `\n`) on the child's stdin (host to extension) and stdout (extension to host). The child writes nothing else to stdout.
- stderr is the extension log. pi prints each line with the prefix `[<extension path>]` to the extension runtime's stderr. In `tui` mode, after the first event, pi shows each line in the chat as a notification instead: lines that start with `error:` as errors, `warn:` as warnings, `info:` and `debug:` as status lines, and other lines as warnings. The level prefix is removed.
- Both sides send requests, responses, and notifications. Each side numbers its own request ids.
- `$/cancelRequest` `{ "id": <request id> }` cancels a request of the other side. The receiver still answers it: with a result, or with error `-32800`.
- An extension-to-host method sent as a notification (no `id`) runs, and the host drops its result.
- Error codes: `-32700` parse error, `-32600` invalid request, `-32601` unknown method, `-32602` invalid params, `-32603` internal error, `-32800` cancelled, `-32001` no UI, `-32002` no context (no event reached the extension within 30 seconds), `-32003` the project is not trusted, `-32004` the MCP server is not connected, `-32005` timed out. A request that needs a ctx and arrives before the first event waits for that event; other requests are answered at once, also during `initialize`. The bridge takes the ctx of `session_start` for every extension.
- Requests that need a ctx: `ui.notify`, `ui.select`, `ui.confirm`, `ui.input`, `ui.editor`, `ui.setStatus`, `ui.setTitle`, `ui.setEditorText`, `ui.getEditorText`, `ui.writeTerminal`, `ui.setWidget`, `ui.setFooter`, `ui.custom`, `session.getEntries`, `session.getBranch`, `model.get`, `model.set`, `model.find`, `ctx.*`, `llm.complete`, `auth.getProviderAuth`, `auth.login`, `settings.set`, and `pi.call` with a target other than `pi`.

## Lifecycle

1. pi starts the child and sends `initialize`. It waits 10 seconds for the answer.
2. The extension answers with its registrations. pi registers them while the extension loads.
3. pi sends the `initialized` notification.
4. pi sends `event`, `tool.execute`, `command.run`, and other requests. The extension sends requests to the host at any time.
5. On `session_shutdown` pi sends `shutdown` (2 second limit), then the `exit` notification, and closes stdin. pi stops the child 2 seconds later when it still runs. An extension also exits when stdin closes.

When the child exits early, pi shows one error notification, fails the pending requests, and removes the extension's tools from the registry and the active tools. `tools.getAll` and the model no longer see them. A reload (`/reload`) starts the child again, and its tools register again and are active again. Renderers of the dead child draw nothing, so pi draws its own rows. pi-rust 0.8.5 and older keep the tools in the registry and answer each call with an error result.

### `initialize` (host to extension)

Params: `protocolVersion` (integer), `piVersion`, `cwd`, `agentDir`, `extensionPath`, `platform` (`process.platform`), `mode` (`tui`, `rpc`, `json`, `print`), `hasUI`, `flags`, `features`.

`features` lists the optional parts of this document that the host speaks: `toolRenderers`, `render.spinner`, `render.refresh`, `ui.getTheme`, `theme_changed`, `ui.working`, `ui.chatStyle`, `contextEntries` (a `resources_discover` result can have `contextEntries`, see [Deferred context](../../crates/pi-coding-agent/docs/deferred-context.md#extension-api-context-entries)), `toolResultExpander`, and `providerAuth` (the `before_provider_auth` event, see [pi-rust events](#pi-rust-events)). Older hosts send no `features`; treat a missing list as empty.

pi loads extensions before it binds the session. `mode` and `hasUI` are the mode the session binds the extensions in (`tui` in interactive mode, and `rpc`, `json`, or `print` for the CLI modes). Hosts that do not know the mode at load time, for example SDK sessions, send `print` and `false`. The ctx snapshot of each event has the current values. `flags` holds the values pi has at load time and is empty for flags this extension declares in its answer. Read flag values from `ctx.flags` or with `flags.get`.

Result:

| Field | Shape |
| --- | --- |
| `protocolVersion` | integer the extension speaks |
| `tools` | `[{ name, label?, description, parameters (JSON Schema object), promptSnippet?, promptGuidelines?: string[], executionMode?, prepareArguments?: boolean, renderCall?: boolean, renderResult?: boolean, renderShell?: "default" \| "self" }]`, see [Tools](#tools) and [Renderers](#renderers) |
| `commands` | `[{ name, description?, completions?: boolean }]` |
| `shortcuts` | `[{ shortcut, description? }]` (for example `ctrl+shift+t`) |
| `flags` | `[{ name, description?, type: "boolean" \| "string", default? }]` |
| `events` | event names to receive, for example `["session_start", "tool_call"]` |
| `providers` | `[{ name, config }]`, see `provider.register` |
| `virtualModels` | `[definition]`, see `virtualModel.register` |
| `autocomplete` | `[{ triggerCharacters: ["@"] }]` |
| `messageRenderers` | custom message types the extension draws, see [Renderers](#renderers) |
| `entryRenderers` | custom session entry types the extension draws |
| `toolRenderers` | `[{ toolName, renderCall?: boolean, renderResult?: boolean, renderShell?: "default" \| "self" }]`: rows of tools that this extension does not register, see [Renderers](#renderers) |
| `toolResultExpander` | `{ detailsKey, maxHeight?, enabled? }`: draws the expanded body of results of any tool whose `details` hold `detailsKey`, see [Renderers](#renderers) |

All fields except `protocolVersion` are optional.

### Versioning

- `protocolVersion` is an integer. This document is version 1.
- The host sends the highest version it speaks. The extension answers with the version it uses. When the host does not speak that version, the load fails with `native extension speaks protocol <n>; pi speaks 1..<m>`.
- New optional fields, new methods, and new events do not change the version. Both sides ignore fields they do not know and answer `-32601` to methods they do not know.
- A removed field, a changed meaning, or a new required field increases the version.

## Host to extension

| Method | Params | Result |
| --- | --- | --- |
| `event` | `{ name, event, ctx }` | `{ result?, patch? }` |
| `tool.execute` | `{ toolCallId, name, params, ctx }` | `{ content, details?, isError? }` |
| `command.run` | `{ name, args, ctx }` | `null` |
| `command.complete` | `{ name, prefix }` | `[{ value, label?, description? }]` or `null` |
| `shortcut.run` | `{ shortcut, ctx }` | `null` |
| `autocomplete.suggest` | `{ lines, cursorLine, cursorCol, force }` | `{ items, prefix, merge? }` or `null` (pi's own suggestions) |
| `virtualModel.route` | `{ provider, id, request }` | the route result of the JavaScript API |
| `tool.prepare` | `{ name, args }` | the prepared arguments |
| `render.toolCall` | `{ toolName, toolCallId, args, context, width }` | `{ lines }` or `null` |
| `render.toolResult` | `{ toolName, toolCallId, result, options, context, width }` | `{ lines }` or `null` |
| `render.toolResultExpanded` | `{ toolName, toolCallId, result, options, context, width }` | `{ lines }` or `null` |
| `render.message` | `{ customType, message, options, width }` | `{ lines }` or `null` |
| `render.entry` | `{ customType, entry, options, width }` | `{ lines }` or `null` |
| `shutdown` | `{}` | `null` |

Notifications: `initialized`, `exit`, `events.message` `{ channel, data }`, `ui.resize` `{ surface, width, height? }`, `ui.key` `{ surface, data, key }`, `ui.closed` `{ surface }`, `footer.changed` `{ footer }`, `theme.changed` `{ theme }`, `$/cancelRequest`.

- `autocomplete.suggest`: with `merge: true`, pi adds its own suggestions after the extension's items when both use the same `prefix`. When the prefixes differ, pi shows only the extension's items.
- `footer.changed`: pi sends it to an extension that lists `footer_changed` in `events`, when the `footer` of the ctx snapshot changes in `tui` mode. pi checks the footer data 4 times per second, so it sends at most 4 notifications per second.
- `theme.changed`: pi sends it to an extension that lists `theme_changed` in `events`, when the theme of the TUI changes in `tui` mode (`/settings`, a system theme that follows the terminal, or an edit of the theme file). `theme` has the shape of the `ui.getTheme` result. pi checks 4 times per second.

### Events

pi sends `event` only for the names the extension subscribed to. `footer_changed` and `theme_changed` are no events: they subscribe to the `footer.changed` and `theme.changed` notifications. `event` is the event object of the JavaScript API without functions and without `signal`. When the event signal aborts, pi sends `$/cancelRequest`.

- `patch` is an RFC 7386 JSON merge patch. pi applies it to the event object in place before the next handler runs. A `null` value removes the key. pi ignores a patch that is not a JSON object and logs a line.
- `result` is the handler return value: `{ block, reason }` for `tool_call`, `{ action: "transform", text }` for `input`, `{ compaction }` or `{ cancel: true }` for `session_before_compact`, `{ message }` for `message_end`, `{ messages }` for `context`, `{ entries, continue }` for `agent_before_settle`, and the other results of upstream pi.

How each event uses a patch:

| Events | Patch |
| --- | --- |
| `tool_call` (`input`), `before_agent_start` (`systemPromptOptions`), `before_provider_request` (`payload`), `before_provider_headers` (`headers`) | pi reads the changed event object. Example: `{ "input": { "command": "ls" } }` for `tool_call`. |
| `tool_result`, `message_end`, `context`, `context_with_system` | When `result` is absent, pi builds it from the patched event: `{ content, details, structuredContent, isError, usage }` (the keys the event has) for `tool_result`, `{ message }` for `message_end`, `{ messages }` for the context events. Example: `{ "content": [...] }` for `tool_result`. |
| All other events, including `message_display`, `input`, `turn_end`, `agent_before_settle`, `cache_warming_decision`, `resources_discover`, `user_bash`, `project_trust`, `mcp_elicitation`, `mcp_elicitation_result`, and `session_before_*` | pi reads only `result`; a patch has no effect. |

pi builds no result for `before_agent_start`: a built `systemPrompt` would replace the whole system prompt.

A `resources_discover` result can also have `contextEntries` (pi-rust only): entries such as agent configs that pi lists in the system prompt or defers to `tool_search`. See [Deferred context](../../crates/pi-coding-agent/docs/deferred-context.md#extension-api-context-entries).

`message_display` (pi-rust only) changes how the text of an assistant message shows in `tui` mode. pi sends it once for each complete assistant message that has text, and once for each such message when pi draws a saved session. The event is `{ type, messageId, index, final, delta, text }`. `messageId` is the session entry id of the message, or `null` when pi has not saved it yet. pi sends the full text in one batch, so `index` is `0`, `final` is `true`, and `delta` and `text` are the same. `text` joins the text blocks of the message with a blank line.

- The result `{ text }` replaces the text blocks on the screen. The thinking blocks stay. An empty `text` hides the text. The session, exports, and the model context keep the original text.
- pi shows the original text until the answer arrives. If no answer arrives within 10 seconds, or the handler fails, pi keeps the original text.
- When several handlers listen, each one gets the `text` (and `delta`) that the handler before it returned. A JavaScript extension listens with `pi.on("message_display", handler)` and returns the same result.
- pi stores the answer for each message, so redraws and width changes send no new event. In the `print`, `json`, and `rpc` modes, pi sends no `message_display` event.

`after_provider_response` of a request reaches the extension, and pi waits for its answer, before pi passes on the first stream event of that response. Thus the `message_end` of the response always comes after the answer to its `after_provider_response`.

The ctx snapshot: `{ cwd, mode, hasUI, model, scopedModels, thinkingLevel, sessionId, sessionName, sessionFile, isIdle, isProjectTrusted, contextUsage, terminalWidth, terminalHeight, flags, footer? }`. `model` is `{ provider, id, name, api, baseUrl, compat, contextWindow, maxTokens, reasoning }` or `null`; it has no headers and no keys. `scopedModels` is `[{ provider, id, thinkingLevel? }]` (the `--models` scope, empty when there is none). `contextUsage` is `{ tokens, contextWindow, percent }` or `null`. `terminalWidth` is the terminal width in columns in `tui` mode, else `null`. `terminalHeight` is the terminal height in rows in `tui` mode, else `null`; it is also `null` when the host does not know the height. `flags` maps each flag of this extension to its value. `footer` is `{ gitBranch, sessionStats?, runState? }` when the host supplies it.

#### pi-rust events

pi-rust adds these events for hook runners. Upstream pi has no such events. JavaScript extensions receive them with `pi.on(name)` too. Each event object also has `type`.

| Event | Payload | Result |
| --- | --- | --- |
| `instructions_loaded` | `{ path, scope: "user" \| "project", reason }`. pi sends one for each context file (`AGENTS.md`, `CLAUDE.md`, and the other names) in the system prompt, after `session_start`. `scope` is `user` for the file of the agent directory. `reason` is the `session_start` reason (`startup`, `reload`, `new`, `resume`, or `fork`). | none |
| `prompt_expansion` | `{ kind: "skill" \| "prompt_template", name, args, source, prompt, text }`, when a prompt `/skill:name args` or `/template args` expands. `source` is the path of the skill or template file, `prompt` is the prompt text, and `text` is the expansion. | `{ action: "block", reason? }` drops the prompt and shows `reason` as a warning. `{ action: "context", text }` appends `"\n\n" + text` to the expansion. The first block wins. The `context` texts of all handlers join in handler order. pi reads only `result`. |
| `notification` | `{ message, level: "info" \| "warning" \| "error", source: "extension" \| "pi", extension? }`, for each `ui.notify` of an extension and each notice of pi itself. `extension` is the `extensionPath` of a native extension that sent the notice. Notices sent while `notification` handlers run make no `notification` event, so a handler that calls `ui.notify` does not loop. | none |
| `tool_call_blocked` | `{ toolCallId, toolName, input, reason, source: "extension" \| "permission", extension? }`, after a `tool_call` handler blocked a call (`extension`) or the ACP permission policy denied it (`permission`). `extension` is the path of the extension that blocked the call. | none |
| `cwd_changed` | `{ oldCwd, newCwd }`, after the session is replaced by a session in another directory. The extensions of the new session get it after their `session_start`. | none |
| `before_provider_auth` | `{ provider, model: { provider, id, api } }`, before each request of the session to a model: agent turns, retries, compaction, and summaries. `llm.complete` and the direct calls of other extensions do not send it. | `{ authProvider }` sends the request with the credential stored under `authProvider` instead of the model's provider, so one model can run on several logins (accounts) of the same subscription. `authProvider` must be a provider that an extension registered with `oauth: { builtin: <the model's provider> }`, for example `anthropic-2` for `anthropic` models. pi refreshes its token as for any login. Any other id fails the request with `<id> is not an account of provider <provider>`. The model, its settings, and its `models.json` headers stay those of the model's provider. The first handler that names an account wins. pi reads only `result`. |

pi does not wait for the handlers of `instructions_loaded`, `notification`, `tool_call_blocked`, and `cwd_changed`.

An account registered with `oauth: { builtin }` and `models: []` adds no models to model lists (`/model`, `--list-models`, RPC, and ACP); `/login` lists it, and `usage.get` reads its quota. A `before_provider_auth` handler then picks it for the models of the base provider, so the models show once while requests use several logins.

During `session_start`, the event context also has `switchSession(sessionPath)` (pi-rust only; upstream pi has it only on command contexts). A native extension calls it with `pi.call` `{ target: "ctx", method: "switchSession", args: [sessionPath] }`. pi queues the switch and returns `null` (`undefined` in JavaScript) at once. When the `session_start` handlers are done, pi switches to the session as a command `ctx.switchSession()` does: `session_before_switch`, new extensions, `session_start` with reason `resume`, and `cwd_changed` when the directory changes. When several handlers queue a switch, the last one wins. After `session_start`, the call fails.

#### MCP elicitation events

These events are pi-rust only. Upstream pi has no MCP elicitation. Native and JavaScript extensions get the same events. With them, an extension can implement hooks such as the `Elicitation` and `ElicitationResult` hooks of Claude Code.

An MCP server sends `elicitation/create` to ask the user for data (form mode) or to open a URL (URL mode). pi announces the `elicitation` client capability with `form` and `url`. pi does these steps:

1. pi sends `mcp_elicitation` to the extensions.
2. If no handler answers, pi shows a dialog. Without a UI (`print` and `json` modes), pi answers `decline`.
3. After the dialog, pi sends `mcp_elicitation_result`.
4. pi sends the answer to the server.

| Event | Payload | Result |
| --- | --- | --- |
| `mcp_elicitation` | `{ type, server, message, mode, url?, elicitationId?, requestedSchema? }` | `{ action, content? }` answers the server, and pi shows no dialog and sends no `mcp_elicitation_result`. If more than one handler answers, a `decline` wins. Otherwise the first answer applies. No result shows the dialog. |
| `mcp_elicitation_result` | `{ type, server, action, mode, elicitationId?, content? }` | `{ action, content? }` replaces the answer of the user. Each handler gets the answer so far. |

- `mode` is `"form"` or `"url"`. A form request has `requestedSchema`: a flat object schema with string, number, integer, boolean, and enum properties. A URL request has `url` and `elicitationId`.
- `action` is `"accept"`, `"decline"`, or `"cancel"`. `content` is the form data of `accept` in form mode. pi removes `content` from other answers.
- Before pi sends an `accept` in form mode, it checks `content` against `requestedSchema`: required properties, unknown properties, and the types of the values. If the check fails, pi sends `decline` and writes a warning line to `mcp.log`.
- A result that is not `undefined` or a valid `{ action, content? }` is an extension error. pi ignores it.

The dialog shows the server name and the message first. In form mode, the user selects **Respond** or **Decline**. Then pi asks for each property: a text input for strings, numbers, integers, and lists, and a selection for booleans and enums. A `default` value fills the input or comes first in the selection. Optional properties can be empty or skipped. Then the user selects **Accept** or **Decline**. In URL mode, pi shows the full URL and its domain, and warns about Punycode domains. The user selects **Open in browser**, **Accept (I will open it myself)**, or **Decline**. pi opens the URL only after the user selects **Open in browser**. Esc in any step sends `cancel`. When the server sends `notifications/elicitation/complete` for an accepted URL request, pi shows a notification.

### Tools

During `tool.execute` the extension sends `tool.update` notifications `{ toolCallId, partial: { content, details? } }`. When the agent aborts the call, pi sends `$/cancelRequest` and waits 5 seconds for the answer; after that the call fails with `Tool call aborted`.

A tool with `prepareArguments: true` gets `tool.prepare` for each call before pi validates the arguments against `parameters` (upstream `prepareArguments`). The answer replaces the arguments, and pi validates and runs the tool with them. An error answer, or no answer within 1.5 seconds, fails the call with that error. pi-rust 0.8.5 and older ignore the flag.

### Renderers

A tool with `renderCall: true` or `renderResult: true` draws its own call or result row in `tui` mode (upstream `renderCall` and `renderResult`). `messageRenderers` and `entryRenderers` draw custom messages (`session.sendMessage`) and custom session entries (`session.appendEntry`) of the listed types (upstream `registerMessageRenderer` and `registerEntryRenderer`). pi-rust 0.8.5 and older ignore these fields.

- pi sends a `render.*` request when a row needs lines for a width it has not drawn. The answer is `{ lines: string[] }`; lines can have ANSI styles, and pi cuts each one to `width` columns. `null` draws no lines.
- pi never waits for the answer. It draws the last lines of the row, or nothing for a new row, and draws again when the answer arrives. Each row has at most one request open. pi keeps the lines of the last 4 widths for each input, so a redraw at a known width sends no request. When the inputs change, for example when a result arrives, or when pi redraws the whole screen (for example after a theme change), pi sends a new request, shows the old lines until the answer arrives, and drops older answers.
- `context` of the tool requests: `{ args, toolCallId, cwd, executionStarted, argsComplete, isPartial, expanded, showImages, isError }`. `options` of `render.toolResult`: `{ expanded, isPartial }`. `options` of `render.message` and `render.entry`: `{ expanded }`.
- `renderShell: "self"` tells pi not to draw the tool frame around the rows.
- `toolRenderers` draw the rows of a tool that pi or another extension registers, for example the built-in `bash` or `read`. They apply only while no other renderer draws that tool: when the tool's own definition, or a `registerToolRenderer` of a JavaScript extension, has a `renderCall` or `renderResult`, pi uses those. A renderer slot that the entry does not set falls back to pi's own row. `register.toolRenderer` takes one entry later.
- `toolResultExpander` (feature `toolResultExpander`): while a tool row is expanded (the `app.tools.expand` key, or a click on the row) and its final `result.details` hold `detailsKey`, pi draws the lines of `render.toolResultExpanded` in place of the result body, below the row's own call line. The tool's renderer draws the row again on collapse. With `maxHeight` above 0 (`options.maxHeight`), pi shows that many lines, the last one being a position line such as `lines 1-29 of 300`; in fullscreen mode the mouse wheel over the body scrolls it, and collapsing returns to the top. The lines come from the extension and never reach the model. `register.toolResultExpander` takes the same object later and replaces the settings; `enabled: false` turns the expander off. Hosts without the feature ignore the field.
- `render.spinner`: the zero-width token `\u001b_pi:spinner\u0007` (an APC string) in an answered line is replaced by the current frame of pi's spinner (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, 80 ms) on each frame, so a row can animate without a request per frame. Styles around the token apply to the frame. Remove the token, for example when the tool finishes, to stop the animation.
- `render.refresh`: an answer `{ lines, refreshMs }` asks pi to send the request again after `refreshMs` milliseconds (at least 250) while the inputs of the row stay the same, for example for an elapsed time. pi keeps the old lines until the new answer arrives. An answer without `refreshMs` ends the refreshes.
- A failed request is logged; the row keeps its last lines until its inputs change.
- Key hints: `keybindings.get` `{ ids? }` returns `{ "<keybinding id>": "<keys>" }`, for example `{ "app.tools.expand": "ctrl+o" }`, with the keys of the user's keybindings (upstream `keyText` of the render context, `9376c8d`). Without `ids`, it returns every app keybinding. An empty string means that the keybinding has no keys.

## Extension to host

### UI

| Method | Params | Result |
| --- | --- | --- |
| `ui.notify` | `{ message, type?: "info" \| "warning" \| "error" }` | `null` |
| `ui.select` | `{ title, options: string[], timeout? }` | string or `null` |
| `ui.confirm` | `{ title, message, timeout? }` | boolean |
| `ui.input` | `{ title, placeholder?, prefill?, secret?, timeout? }` | string or `null` |
| `ui.editor` | `{ title, prefill? }` | string or `null` |
| `ui.setStatus` | `{ key, text \| null }` | `null` |
| `ui.setWidget` | `{ key, lines: string[] \| null, placement?: "aboveEditor" \| "belowEditor" }` | `null` |
| `ui.setFooter` | `{ lines: string[] \| null }` | `null` |
| `ui.setTitle` | `{ title }` | `null` |
| `ui.setWorkingMessage` | `{ message? }` | `null` |
| `ui.setWorkingIndicator` | `{ frames?: string[], intervalMs? }` | `null` |
| `ui.setWorkingVisible` | `{ visible }` | `null` |
| `ui.setHiddenThinkingLabel` | `{ label? }` | `null` |
| `ui.setChatStyle` | `{ userPrefix?, assistantPrefix?, collapseThinking?, roundedEditor?, editorBorderColor?, promptLayout? }` | `null` |
| `ui.getTheme` | `{}` | the theme, see below |
| `ui.setEditorText` | `{ text }` | `null` |
| `ui.getEditorText` | `{}` | string |
| `ui.writeTerminal` | `{ data, tmuxPassthrough? }` | `true` when pi wrote to the terminal (`tui` mode), else `false` |
| `ui.custom` | `{ surface, lines, overlay?, overlayOptions? }` | `{ result }` when the panel closes |
| `ui.update` | `{ surface, lines }` | `null` |
| `ui.close` | `{ surface, result? }` | `null` |

Dialogs and `ui.custom` return error `-32001` when pi has no UI. `$/cancelRequest` closes an open dialog.

Working indicator (`ui.working`, upstream `ctx.ui.setWorkingMessage`, `setWorkingIndicator`, `setWorkingVisible`, and `setHiddenThinkingLabel`): the indicator shows while the agent works. Without `message` or `label`, pi shows its own text again. `frames` are drawn as they are, so they can carry their own styles; without `frames`, pi draws its own spinner.

Chat style (`ui.chatStyle`, pi-rust `ctx.ui.setChatStyle`): `userPrefix` goes before the first line of each user message and `assistantPrefix` before the first line of the text of each assistant message; the other lines get the same indent. `collapseThinking: true` shows finished thinking blocks as the hidden thinking label, and keeps the block being written open; a click opens one block, and showing thinking blocks with the thinking toggle (`app.thinking.toggle`) opens them all. `roundedEditor: true` rounds the editor corners and draws side bars in the editor padding (`editorPaddingX`). Each call replaces the whole style, and a reload resets it.

`editorBorderColor` sets the editor border color in every prompt layout: a theme color token (a foreground name of a theme file, such as `accent`, `border`, `borderAccent`, `borderMuted`, `success`, or `warning`) or a `#rrggbb` color. While it is set, the border keeps this color in place of the thinking level and bash mode colors. Without it, or with an empty or unknown value, pi colors the border as before.

`promptLayout` sets the frame around the editor text. Without it, `roundedEditor` decides the corners.

| Value | Editor |
| --- | --- |
| `frame` | The rounded editor of `roundedEditor: true`. |
| `caret` | A rounded box on all four sides with a column of padding inside (`editorPaddingX` when larger) and a two-column gutter. The first text row starts with a bold accent `›`, and wrapped and later rows start under the text. An empty editor shows a muted placeholder with the cursor on its first cell: `Ask anything · / for commands · @ for files`, or `Queue a message · esc to stop the agent` while the agent runs. The working status and the `↑ N more` labels stay in the border. |
| `harbor` | `caret` after one blank line. The border rules hold no labels: the working status shows above the editor, and the gutter shows `↑` and `↓` on the first and last rows when rows are hidden. The border uses the theme `border` color unless `editorBorderColor` is set. |

Autocomplete opens under the box, aligned with the text. A terminal narrower than six columns shows the plain editor.

`ui.getTheme` returns the theme that the TUI draws with: `{ name, sourcePath, appearance: "dark" | "light", mode: "truecolor" | "256color", fg: { <token>: <SGR open sequence> }, bg: { <token>: <SGR open sequence> }, dim: [<token>] }`. The tokens are those of the theme JSON, for example `accent`, `muted`, `toolTitle`, and `toolSuccessBg`. A token in `dim` also needs `\u001b[2m`. Close a foreground with `\u001b[39m` (and `\u001b[22m` after dim) and a background with `\u001b[49m`. It needs no ctx.

`ui.input`: `prefill` is the initial text. `secret: true` shows `*` for each character in `tui` mode. In `rpc` mode pi adds `prefill` and `secret` to the `input` request of the client.

Line-based surfaces: a widget (surface `widget:<key>`), the footer (surface `footer`), and a `ui.custom` panel (surface chosen by the extension). The host renders the last lines the extension pushed, cut to the render width. Each push redraws the screen. When the render width or the terminal height changes, the host sends `ui.resize`: `width` is the render width in columns, and `height` is the number of terminal rows. `height` is a positive integer; the host omits it when it does not know the height. A panel can use `height` to keep its lines on the screen. Hosts before this field send only `width` and do not send `ui.resize` for a height change. A panel gets each key as `ui.key`: `data` is the raw terminal input, and `key` is the key id of pi-tui `parseKey` (for example `shift+enter`, `ctrl+c`, `q`) or `null`. The extension closes a panel with `ui.close`; the `ui.custom` request then answers `{ result }`. When the extension cancels an open `ui.custom` with `$/cancelRequest`, pi closes the panel, sends `ui.closed`, and the request answers `{ result: null }`. `ui.close` on a widget or the footer removes it. In modes other than `tui`, widgets go to pi as plain lines and get no `ui.resize`.

### Session, model, and tools

| Method | Params | Result |
| --- | --- | --- |
| `session.sendMessage` | `{ customType, content, display?, details?, deliverAs?: "steer" \| "followUp" \| "nextTurn", triggerTurn? }` | `null` |
| `session.sendUserMessage` | `{ content, deliverAs? }` | `null` |
| `session.appendEntry` | `{ customType, data? }` | `null` |
| `session.getEntries` | `{}` | entries |
| `session.getBranch` | `{ fromId? }` | entries from the root to the leaf |
| `session.setName` / `session.getName` | `{ name }` / `{}` | `null` / string or `null` |
| `model.get` / `model.set` | `{}` / `{ provider, id }` | model or `null` / boolean |
| `model.find` | `{ provider, id }` | model or `null` |
| `thinking.get` / `thinking.set` | `{}` / `{ level }` | level / `null` |
| `tools.getActive` / `tools.setActive` | `{}` / `{ names }` | names / `null` |
| `tools.getAll` / `commands.getAll` | `{}` | tool infos / command infos |
| `flags.get` | `{ name }` | value or `null` |
| `settings.get` | `{ namespace? }` | the merged global and project settings, or their `namespace` key |
| `settings.set` | `{ namespace, value, scope?: "global" \| "project" }` | `null` |
| `keybindings.get` | `{ ids?: string[] }` | key text by keybinding id, see [Renderers](#renderers) |

### Settings

`settings.set` writes `value` as the top-level key `namespace` of the global settings (`<agentDir>/settings.json`, the default scope) or the project settings (`<cwd>/.pi/settings.json`). pi keeps the other keys of the file. `value: null` removes the key. `settings.get { namespace }` reads the merged value at once, with the project value over the global one.

- The project scope needs a trusted project; else the error is `-32003` and pi writes nothing.
- A key of pi's own settings (for example `defaultModel`, `extensions`, or `packages`) is refused with `-32602`, as are an empty namespace and another scope.
- Upstream pi has no settings write for extensions. pi-rust 0.8.5 and older answer `-32601`.

### Context

| Method | Params | Result |
| --- | --- | --- |
| `ctx.compact` | `{ customInstructions? }` | the compaction result, when it completes |
| `ctx.abort` | `{}` | `null` |
| `ctx.isIdle` | `{}` | boolean |
| `ctx.getContextUsage` | `{}` | usage or `null` |
| `ctx.getSystemPrompt` | `{}` | string |
| `ctx.getContextBreakdown` | `{}` | `{ parts: [{ id, label, tokens }], total }` |

`ctx.getContextBreakdown` is a pi-rust extension, also on the JavaScript `ctx`. It returns the estimated tokens of each part of the next request, in the order `/context` shows them. The ids are `systemPrompt`, `builtinTools`, `extensionTools`, `mcpTools`, `contextFiles`, `skills`, `userMessages`, `assistantMessages`, and `toolResults`. The parts are estimates, so their total can differ from the `tokens` of `ctx.getContextUsage`. Upstream pi and older pi-rust releases have no such method.

### Models, auth, events, and processes

| Method | Params | Result |
| --- | --- | --- |
| `llm.complete` | `{ model?: "provider/id", systemPrompt?, prompt? \| messages?, maxTokens?, temperature?, reasoning?, headers? }` | `{ message, text }` |
| `auth.getProviderAuth` | `{ provider }` | `{ apiKey, headers, source }` or `null` |
| `auth.login` | `{ provider }` | `true` after a login, `false` when the user cancels it |
| `usage.get` | `{ provider?, refresh? }` | `{ providers: [{ provider, plan, windows: [{ label, usedPercent, resetsAt, windowMinutes, windowSeconds?, scope, families?, id?, resetsAtMs? }], credits?, error, errorKind?, retryAfterMs?, fetchedAt }] }` |
| `events.emit` | `{ channel, data }` | `null` |
| `events.subscribe` / `events.unsubscribe` | `{ channel }` | `null` |
| `exec` | `{ command, args?, cwd?, timeout?, stdin?, maxBuffer? }` | `{ stdout, stderr, code, killed, truncated }` |
| `provider.register` | `{ name, config }` | `null` |
| `provider.unregister` | `{ name }` | `null` |
| `virtualModel.register` | `{ provider, id, name?, ... }` | `null` |
| `virtualModel.unregister` | `{ provider, id }` | `null` |
| `pi.call` | `{ target: "pi" \| "ctx" \| "ctx.ui" \| "ctx.sessionManager" \| "ctx.modelRegistry", method, args?: [] }` | the awaited return value as JSON |

- `llm.complete` uses the current model, or `provider/id`, with the credentials of pi. The key stays in pi. `prompt` is one user message; `messages` are pi-ai messages. `headers` is an object of string values (else `-32602`) that goes on the request over the headers of pi; a header name of the request replaces the header of pi with the same name, compared without case. `text` joins the text blocks of the answer.
- `usage.get` returns the subscription limits of pi's logins, as `/usage` shows them. Without `provider`, the list has one entry for each logged-in provider that has a usage fetcher: `anthropic`, `openai-codex`, `cursor`, `xai`, `google-antigravity`, and the providers registered with `oauth: { builtin }` for one of the first four. The new fields are in [Subscription usage limits](../providers/usage.md#what-pi-shows). With `provider`, the list has one entry for that provider. pi keeps each result for 60 seconds (5 minutes for `google-antigravity`); `refresh: true` fetches again.
  - `plan` is the plan name, or `null`. `fetchedAt` is the fetch time in Unix milliseconds.
  - `usedPercent` is a number from 0 to 100. `resetsAt` is an ISO 8601 UTC time (`2026-10-30T17:00:00Z`) or `null`. `windowMinutes` is the window length (`300` for 5 hours, `10080` for a week) or `null`.
  - `error` is a message, or `null`. A provider without a login, without a usage fetcher, or whose request fails has `error` and no windows. The method itself does not fail for these.
  - pi-rust 0.8.2 and older answer `-32601`.
- `model.find` returns the model in the shape of the ctx snapshot `model`, or `null` when `provider` or `id` is missing or is not a string.
- `exec` writes `stdin` to the standard input of the process and then closes it. Without `stdin`, the process gets no standard input.
- `exec` runs the command without a shell, as `pi.exec` does. `signal` (the request's `$/cancelRequest`) and `timeout` stop it, and a command that ignores SIGTERM gets SIGKILL after 5 seconds. `maxBuffer` limits stdout and stderr together: an integer from 0 to the string limit of the runtime (default 256 MiB), else `-32602`. Over the limit pi stops the command and answers the output up to the limit with `truncated: true`, `killed: true`, and a nonzero `code`; an incomplete UTF-8 character at the cut is dropped. `killed` is true whenever pi stopped the command. pi-rust 0.8.6 and older cap the output at 16 MiB with a marker in `stderr`, and send no `truncated`.
- `events.subscribe` delivers each `pi.events` message on `channel` as an `events.message` notification.
- `provider.register` takes the declarative part of `pi.registerProvider`: `name`, `baseUrl`, `api` (an existing API id), `apiKey`, `headers`, `models`, and `oauth: { builtin, name? }`. Functions are not available. See [Providers](#providers).
- `virtualModel.register` takes the definition of `pi.registerVirtualModel`; pi sends `virtualModel.route` for each request.

### MCP

| Method | Params | Result |
| --- | --- | --- |
| `mcp.callTool` | `{ server, tool, args?, timeout? }` | `{ content, isError, structuredContent? }` |

`mcp.callTool` calls a tool of an MCP server of the session outside of a tool call, for example for a hook. `server` is the server name of `mcp.json` or of `pi.registerMcpServer`, and `tool` is the tool name that the server lists (not `mcp__<server>__<tool>`). `args` is an object (default `{}`). `content` holds the MCP content blocks of the result. A result with `isError: true` is an answer, not an error.

- The call does not go through the `tool_call` and `tool_result` events, and pi does not write it to the session. It works while the agent is idle or streaming.
- When the server is still connecting, pi waits for it. `timeout` (milliseconds) limits the wait and the call together. Without it, pi waits until the connection try ends, and the call uses the `timeout` of the server config.
- pi never starts a sign-in. A server that needs a sign-in fails with `-32004`.
- Errors: `-32602` for an unknown server, an unknown tool, or invalid params; `-32004` for a server that is disabled, failed, or needs a sign-in, and when MCP support is off; `-32005` when `timeout` expires; `-32800` after `$/cancelRequest`; `-32603` when the server fails the request.
- Upstream pi has no such method. pi-rust 0.12.1 and older answer `-32601`.
- In JavaScript and TypeScript extensions, `pi.callMcpTool(server, tool, args?, { timeout?, signal? })` returns the same object. Its errors have the code as `error.code`.

### Providers

`oauth: { builtin: "<provider id>", name? }` gives the provider a login flow of pi, for example `anthropic` or `openai-codex` (the ids of `pi login`). `/login` lists the provider under `name` (else the provider id). pi runs the login, stores the credential under the id of the extension provider (`anthropic-2` in this example), refreshes it when it expires, and sends requests of the provider's models with it. The extension sees no tokens; `auth.getProviderAuth` returns the current key.

`auth.login { provider }` opens the `/login` dialog of pi for a provider that this extension registered with `oauth`, for example after a user picks "add account" in a menu of the extension. It answers when the dialog closes: `true` after a login, `false` when the user cancels it or the session ends. A provider of another extension or of pi is refused with `-32602`, a mode other than `tui` with `-32001`, and a failed login with `-32603` and the message of pi. pi-rust 0.8.5 and older answer `-32601`. An unknown id registers nothing; pi reports it to the extension error listeners, as it does for other invalid provider configs, and `provider.register` answers `null`. pi-rust 0.8.5 and older ignore `builtin`, so the provider has no login there.

```json
{ "name": "anthropic-2", "config": { "name": "Anthropic #2", "api": "anthropic-messages", "baseUrl": "https://api.anthropic.com", "models": [], "oauth": { "builtin": "anthropic", "name": "Anthropic #2" } } }
```

### Later registrations

`register.tool` (one tool definition), `register.command`, `register.shortcut`, `register.flag`, `register.events` (`{ events }`, adds subscriptions), and `register.autocomplete` take the same shapes as the `initialize` result fields. `register.messageRenderer` and `register.entryRenderer` take `{ customType }`, `register.toolRenderer` takes one `toolRenderers` entry, and `register.toolResultExpander` takes a `toolResultExpander` object. They answer `null`.

## Example transcript

`>` is host to extension, `<` is extension to host.

```text
> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"piVersion":"0.8.0","cwd":"/work","agentDir":"/home/u/.pi/agent","extensionPath":"native:hello/hello","platform":"darwin","mode":"tui","hasUI":true,"flags":{}}}
< {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"tools":[{"name":"greet","description":"Greet someone","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}],"commands":[{"name":"hello","description":"Say hello"}],"events":["before_agent_start"]}}
> {"jsonrpc":"2.0","method":"initialized"}
> {"jsonrpc":"2.0","id":2,"method":"tool.execute","params":{"toolCallId":"c1","name":"greet","params":{"name":"Ada"},"ctx":{"cwd":"/work","hasUI":true}}}
< {"jsonrpc":"2.0","method":"tool.update","params":{"toolCallId":"c1","partial":{"content":[{"type":"text","text":"working"}]}}}
< {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"Hello, Ada"}]}}
> {"jsonrpc":"2.0","id":3,"method":"event","params":{"name":"before_agent_start","event":{"type":"before_agent_start","prompt":"hi","systemPromptOptions":{"sections":{}}},"ctx":{"cwd":"/work"}}}
< {"jsonrpc":"2.0","id":3,"result":{"patch":{"systemPromptOptions":{"sections":{"hello":"Greet people by name."}}},"result":null}}
> {"jsonrpc":"2.0","id":4,"method":"command.run","params":{"name":"hello","args":"","ctx":{"cwd":"/work"}}}
< {"jsonrpc":"2.0","id":1,"method":"ui.select","params":{"title":"Pick","options":["a","b"]}}
> {"jsonrpc":"2.0","id":1,"result":"b"}
< {"jsonrpc":"2.0","id":2,"method":"ui.custom","params":{"surface":"p1","lines":["Panel","q closes"]}}
> {"jsonrpc":"2.0","method":"ui.resize","params":{"surface":"p1","width":80,"height":24}}
> {"jsonrpc":"2.0","method":"ui.key","params":{"surface":"p1","data":"q"}}
< {"jsonrpc":"2.0","method":"ui.close","params":{"surface":"p1","result":"closed"}}
> {"jsonrpc":"2.0","id":2,"result":{"result":"closed"}}
< {"jsonrpc":"2.0","id":4,"result":null}
> {"jsonrpc":"2.0","id":5,"method":"shutdown","params":{}}
< {"jsonrpc":"2.0","id":5,"result":null}
> {"jsonrpc":"2.0","method":"exit"}
```
