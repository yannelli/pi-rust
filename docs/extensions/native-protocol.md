# Native extension protocol

Created 2026-10-29. Last updated 2026-10-31.

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
- Error codes: `-32700` parse error, `-32600` invalid request, `-32601` unknown method, `-32602` invalid params, `-32603` internal error, `-32800` cancelled, `-32001` no UI, `-32002` no context (no event reached the extension within 30 seconds), `-32003` the project is not trusted. A request that needs a ctx and arrives before the first event waits for that event; other requests are answered at once, also during `initialize`. The bridge takes the ctx of `session_start` for every extension.
- Requests that need a ctx: `ui.notify`, `ui.select`, `ui.confirm`, `ui.input`, `ui.editor`, `ui.setStatus`, `ui.setTitle`, `ui.setEditorText`, `ui.getEditorText`, `ui.writeTerminal`, `ui.setWidget`, `ui.setFooter`, `ui.custom`, `session.getEntries`, `session.getBranch`, `model.get`, `model.set`, `model.find`, `ctx.*`, `llm.complete`, `auth.getProviderAuth`, `auth.login`, `settings.set`, and `pi.call` with a target other than `pi`.

## Lifecycle

1. pi starts the child and sends `initialize`. It waits 10 seconds for the answer.
2. The extension answers with its registrations. pi registers them while the extension loads.
3. pi sends the `initialized` notification.
4. pi sends `event`, `tool.execute`, `command.run`, and other requests. The extension sends requests to the host at any time.
5. On `session_shutdown` pi sends `shutdown` (2 second limit), then the `exit` notification, and closes stdin. pi stops the child 2 seconds later when it still runs. An extension also exits when stdin closes.

When the child exits early, pi shows one error notification, fails the pending requests, and removes the extension's tools from the registry and the active tools. `tools.getAll` and the model no longer see them. A reload (`/reload`) starts the child again, and its tools register again and are active again. Renderers of the dead child draw nothing, so pi draws its own rows. pi-rust 0.8.5 and older keep the tools in the registry and answer each call with an error result.

### `initialize` (host to extension)

Params: `protocolVersion` (integer), `piVersion`, `cwd`, `agentDir`, `extensionPath`, `platform` (`process.platform`), `mode` (`tui`, `rpc`, `json`, `print`), `hasUI`, `flags`.

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
| `render.message` | `{ customType, message, options, width }` | `{ lines }` or `null` |
| `render.entry` | `{ customType, entry, options, width }` | `{ lines }` or `null` |
| `shutdown` | `{}` | `null` |

Notifications: `initialized`, `exit`, `events.message` `{ channel, data }`, `ui.resize` `{ surface, width, height? }`, `ui.key` `{ surface, data, key }`, `ui.closed` `{ surface }`, `footer.changed` `{ footer }`, `$/cancelRequest`.

- `autocomplete.suggest`: with `merge: true`, pi adds its own suggestions after the extension's items when both use the same `prefix`. When the prefixes differ, pi shows only the extension's items.
- `footer.changed`: pi sends it to an extension that lists `footer_changed` in `events`, when the `footer` of the ctx snapshot changes in `tui` mode. pi checks the footer data 4 times per second, so it sends at most 4 notifications per second.

### Events

pi sends `event` only for the names the extension subscribed to. `footer_changed` is no event: it subscribes to the `footer.changed` notification. `event` is the event object of the JavaScript API without functions and without `signal`. When the event signal aborts, pi sends `$/cancelRequest`.

- `patch` is an RFC 7386 JSON merge patch. pi applies it to the event object in place before the next handler runs. A `null` value removes the key. pi ignores a patch that is not a JSON object and logs a line.
- `result` is the handler return value: `{ block, reason }` for `tool_call`, `{ action: "transform", text }` for `input`, `{ compaction }` or `{ cancel: true }` for `session_before_compact`, `{ message }` for `message_end`, `{ messages }` for `context`, `{ entries, continue }` for `agent_before_settle`, and the other results of upstream pi.

How each event uses a patch:

| Events | Patch |
| --- | --- |
| `tool_call` (`input`), `before_agent_start` (`systemPromptOptions`), `before_provider_request` (`payload`), `before_provider_headers` (`headers`) | pi reads the changed event object. Example: `{ "input": { "command": "ls" } }` for `tool_call`. |
| `tool_result`, `message_end`, `context`, `context_with_system` | When `result` is absent, pi builds it from the patched event: `{ content, details, structuredContent, isError, usage }` (the keys the event has) for `tool_result`, `{ message }` for `message_end`, `{ messages }` for the context events. Example: `{ "content": [...] }` for `tool_result`. |
| All other events, including `input`, `turn_end`, `agent_before_settle`, `cache_warming_decision`, `resources_discover`, `user_bash`, `project_trust`, and `session_before_*` | pi reads only `result`; a patch has no effect. |

pi builds no result for `before_agent_start`: a built `systemPrompt` would replace the whole system prompt.

`after_provider_response` of a request reaches the extension, and pi waits for its answer, before pi passes on the first stream event of that response. Thus the `message_end` of the response always comes after the answer to its `after_provider_response`.

The ctx snapshot: `{ cwd, mode, hasUI, model, scopedModels, thinkingLevel, sessionId, sessionName, sessionFile, isIdle, isProjectTrusted, contextUsage, terminalWidth, terminalHeight, flags, footer? }`. `model` is `{ provider, id, name, api, baseUrl, compat, contextWindow, maxTokens, reasoning }` or `null`; it has no headers and no keys. `scopedModels` is `[{ provider, id, thinkingLevel? }]` (the `--models` scope, empty when there is none). `contextUsage` is `{ tokens, contextWindow, percent }` or `null`. `terminalWidth` is the terminal width in columns in `tui` mode, else `null`. `terminalHeight` is the terminal height in rows in `tui` mode, else `null`; it is also `null` when the host does not know the height. `flags` maps each flag of this extension to its value. `footer` is `{ gitBranch, sessionStats?, runState? }` when the host supplies it.

### Tools

During `tool.execute` the extension sends `tool.update` notifications `{ toolCallId, partial: { content, details? } }`. When the agent aborts the call, pi sends `$/cancelRequest` and waits 5 seconds for the answer; after that the call fails with `Tool call aborted`.

A tool with `prepareArguments: true` gets `tool.prepare` for each call before pi validates the arguments against `parameters` (upstream `prepareArguments`). The answer replaces the arguments, and pi validates and runs the tool with them. An error answer, or no answer within 1.5 seconds, fails the call with that error. pi-rust 0.8.5 and older ignore the flag.

### Renderers

A tool with `renderCall: true` or `renderResult: true` draws its own call or result row in `tui` mode (upstream `renderCall` and `renderResult`). `messageRenderers` and `entryRenderers` draw custom messages (`session.sendMessage`) and custom session entries (`session.appendEntry`) of the listed types (upstream `registerMessageRenderer` and `registerEntryRenderer`). pi-rust 0.8.5 and older ignore these fields.

- pi sends a `render.*` request when a row needs lines for a width it has not drawn. The answer is `{ lines: string[] }`; lines can have ANSI styles, and pi cuts each one to `width` columns. `null` draws no lines.
- pi never waits for the answer. It draws the last lines of the row, or nothing for a new row, and draws again when the answer arrives. Each row has at most one request open. pi keeps the lines of the last 4 widths for each input, so a redraw at a known width sends no request. When the inputs change, for example when a result arrives, or when pi redraws the whole screen (for example after a theme change), pi sends a new request, shows the old lines until the answer arrives, and drops older answers.
- `context` of the tool requests: `{ args, toolCallId, cwd, executionStarted, argsComplete, isPartial, expanded, showImages, isError }`. `options` of `render.toolResult`: `{ expanded, isPartial }`. `options` of `render.message` and `render.entry`: `{ expanded }`.
- `renderShell: "self"` tells pi not to draw the tool frame around the rows.
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
| `ui.setEditorText` | `{ text }` | `null` |
| `ui.getEditorText` | `{}` | string |
| `ui.writeTerminal` | `{ data, tmuxPassthrough? }` | `true` when pi wrote to the terminal (`tui` mode), else `false` |
| `ui.custom` | `{ surface, lines, overlay?, overlayOptions? }` | `{ result }` when the panel closes |
| `ui.update` | `{ surface, lines }` | `null` |
| `ui.close` | `{ surface, result? }` | `null` |

Dialogs and `ui.custom` return error `-32001` when pi has no UI. `$/cancelRequest` closes an open dialog.

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

### Models, auth, events, and processes

| Method | Params | Result |
| --- | --- | --- |
| `llm.complete` | `{ model?: "provider/id", systemPrompt?, prompt? \| messages?, maxTokens?, temperature?, reasoning?, headers? }` | `{ message, text }` |
| `auth.getProviderAuth` | `{ provider }` | `{ apiKey, headers, source }` or `null` |
| `auth.login` | `{ provider }` | `true` after a login, `false` when the user cancels it |
| `usage.get` | `{ provider?, refresh? }` | `{ providers: [{ provider, plan, windows: [{ label, usedPercent, resetsAt, windowMinutes, scope, families?, id?, resetsAtMs? }], credits?, error, errorKind?, retryAfterMs?, fetchedAt }] }` |
| `events.emit` | `{ channel, data }` | `null` |
| `events.subscribe` / `events.unsubscribe` | `{ channel }` | `null` |
| `exec` | `{ command, args?, cwd?, timeout?, stdin? }` | `{ stdout, stderr, code, killed }` |
| `provider.register` | `{ name, config }` | `null` |
| `provider.unregister` | `{ name }` | `null` |
| `virtualModel.register` | `{ provider, id, name?, ... }` | `null` |
| `virtualModel.unregister` | `{ provider, id }` | `null` |
| `pi.call` | `{ target: "pi" \| "ctx" \| "ctx.ui" \| "ctx.sessionManager" \| "ctx.modelRegistry", method, args?: [] }` | the awaited return value as JSON |

- `llm.complete` uses the current model, or `provider/id`, with the credentials of pi. The key stays in pi. `prompt` is one user message; `messages` are pi-ai messages. `headers` is an object of string values (else `-32602`) that goes on the request over the headers of pi; a header name of the request replaces the header of pi with the same name, compared without case. `text` joins the text blocks of the answer.
- `usage.get` returns the subscription limits of pi's logins, as `/usage` shows them. Without `provider`, the list has one entry for each logged-in provider that has a usage fetcher: `anthropic`, `openai-codex`, `cursor`, `xai`, `google-antigravity`, and the providers registered with `oauth: { builtin }` for one of them. The new fields are in [Subscription usage limits](../providers/usage.md#what-pi-shows). With `provider`, the list has one entry for that provider. pi keeps each result for 60 seconds (5 minutes for `google-antigravity`); `refresh: true` fetches again.
  - `plan` is the plan name, or `null`. `fetchedAt` is the fetch time in Unix milliseconds.
  - `usedPercent` is a number from 0 to 100. `resetsAt` is an ISO 8601 UTC time (`2026-10-30T17:00:00Z`) or `null`. `windowMinutes` is the window length (`300` for 5 hours, `10080` for a week) or `null`.
  - `error` is a message, or `null`. A provider without a login, without a usage fetcher, or whose request fails has `error` and no windows. The method itself does not fail for these.
  - pi-rust 0.8.2 and older answer `-32601`.
- `model.find` returns the model in the shape of the ctx snapshot `model`, or `null` when `provider` or `id` is missing or is not a string.
- `exec` writes `stdin` to the standard input of the process and then closes it. Without `stdin`, the process gets no standard input.
- `events.subscribe` delivers each `pi.events` message on `channel` as an `events.message` notification.
- `provider.register` takes the declarative part of `pi.registerProvider`: `name`, `baseUrl`, `api` (an existing API id), `apiKey`, `headers`, `models`, and `oauth: { builtin, name? }`. Functions are not available. See [Providers](#providers).
- `virtualModel.register` takes the definition of `pi.registerVirtualModel`; pi sends `virtualModel.route` for each request.

### Providers

`oauth: { builtin: "<provider id>", name? }` gives the provider a login flow of pi, for example `anthropic` or `openai-codex` (the ids of `pi login`). `/login` lists the provider under `name` (else the provider id). pi runs the login, stores the credential under the id of the extension provider (`anthropic-2` in this example), refreshes it when it expires, and sends requests of the provider's models with it. The extension sees no tokens; `auth.getProviderAuth` returns the current key.

`auth.login { provider }` opens the `/login` dialog of pi for a provider that this extension registered with `oauth`, for example after a user picks "add account" in a menu of the extension. It answers when the dialog closes: `true` after a login, `false` when the user cancels it or the session ends. A provider of another extension or of pi is refused with `-32602`, a mode other than `tui` with `-32001`, and a failed login with `-32603` and the message of pi. pi-rust 0.8.5 and older answer `-32601`. An unknown id registers nothing; pi reports it to the extension error listeners, as it does for other invalid provider configs, and `provider.register` answers `null`. pi-rust 0.8.5 and older ignore `builtin`, so the provider has no login there.

```json
{ "name": "anthropic-2", "config": { "name": "Anthropic #2", "api": "anthropic-messages", "baseUrl": "https://api.anthropic.com", "models": [], "oauth": { "builtin": "anthropic", "name": "Anthropic #2" } } }
```

### Later registrations

`register.tool` (one tool definition), `register.command`, `register.shortcut`, `register.flag`, `register.events` (`{ events }`, adds subscriptions), and `register.autocomplete` take the same shapes as the `initialize` result fields. `register.messageRenderer` and `register.entryRenderer` take `{ customType }`. They answer `null`.

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
