# SDK child session controls

Created: 2026-11-01. Last updated: 2026-11-24.

This file records which members of the SDK `AgentSession` that `createAgentSession` returns run in pi-rust, compared with upstream `packages/coding-agent/src/core/agent-session.ts` at `35dfa757378bd7a8570670e99c2687bdd1039a06`.

## How it works

- Each member calls the matching native `AgentSession` method through `sdk.read`, `sdk.control`, or an async `sdk.<name>` op (`crates/pi-coding-agent/src/extensions/ops_sdk/child.rs`). `js/sdk_session.js` passes arguments and results only.
- `subscribe` stays active until its unsubscribe function, the last unsubscribe, or `dispose`. Events outside a prompt reach the listeners. Rust keeps one session listener per child and one task that calls the JavaScript callback in order. Unsubscribing the last listener, `dispose`, and the shutdown of the extension runner remove the listener and stop the task.
- `prompt` resolves after its events reach the listeners. When a listener throws, the run is aborted and `prompt` rejects with that error.
- `setModel`, `cycleModel`, `setThinkingLevel`, and `cycleThinkingLevel` save to settings only with `{ persist: true }`, as upstream. The `ctx` actions of extensions keep their own defaults.
- `customTools` definitions stay in the host of the caller (`js/sdk_tools.js`). Rust registers each as an `ExtensionTool` with the key of its definition (`ExtensionTool::sdk`), so `execute`, `prepareArguments`, `reachesTools`, and `prepareLoadout` run in the caller's host. `execute` gets a tool context of the child session: `ctx.cwd`, `ctx.sessionManager`, `ctx.tools`, and `ctx.executeTool` act on the child. As upstream, a custom tool wins over an extension tool and a built-in tool of the same name, the `tools` allowlist filters it, and `getAllTools` reports it with the source `sdk` and the path `<sdk:name>`. `dispose` drops the definitions. A child session has no UI, so `renderCall`, `renderResult`, and `renderShell` are not used.
- A `modelRuntime` from `ModelRuntime.create`, `session.modelRuntime`, or `ctx.modelRegistry.runtime` selects that runtime; `ctx.modelRegistry.runtime` is the runtime of the host. Options that upstream does not define, such as the `modelRegistry` of older pi versions, are ignored, as upstream.

## Members

| Member | State |
| --- | --- |
| `prompt` with `images`, `expandPromptTemplates`, `source`, `streamingBehavior` | Works. Without `streamingBehavior` while streaming, it throws the upstream message. |
| `steer`, `followUp` (with `images` and `source`) | Work; they return `"queued"` or `"handled"`. |
| `clearQueue`, `pendingMessageCount`, `getSteeringMessages`, `getFollowUpMessages` | Work. |
| `abort`, `waitForIdle`, `isIdle`, `isStreaming`, `isCompacting` | Work. |
| `setModel`, `cycleModel`, `setThinkingLevel`, `cycleThinkingLevel`, `getAvailableThinkingLevels`, `supportsThinking` | Work. |
| `steeringMode`, `followUpMode`, `setSteeringMode`, `setFollowUpMode` | Work; the setters save the mode, as upstream. |
| `compact`, `abortCompaction`, `abortBranchSummary`, `autoCompactionEnabled`, `setAutoCompactionEnabled` | Work. |
| `retryAttempt`, `isRetrying`, `abortRetry`, `autoRetryEnabled`, `setAutoRetryEnabled` | Work. |
| `setCacheWarmingMode` | Works. |
| `isBashRunning`, `hasPendingBashMessages`, `abortBash` | Work. |
| `sessionName`, `setSessionName`, `getSessionStats`, `getUserMessagesForForking`, `getLastAssistantText`, `refreshContext`, `exportToJsonl`, `exportToHtml` | Work. `exportToHtml` rejects options, so `themeName` is not supported. |
| `getActiveToolNames`, `getCallableToolNames`, `getAllTools`, `setActiveToolsByName` | Work. `getAllTools` entries have no `reachesTools`. |
| `state` | Returns `systemPrompt`, `model`, `thinkingLevel`, `tools`, `messages`, `isStreaming`, `streamingMessage`, `pendingToolCalls` (a `Set`), and `errorMessage`. `tools` holds tool infos without `execute`; each read is a new copy. |
| `subscribe` | Works for the whole session. |
| `scopedModels`, `setScopedModels`, and the `createAgentSession` option `scopedModels` | Work on the native scope: `cycleModel` uses it with its thinking levels, and `setModel` with `{ persist: true }` adds the model to a non-empty scope and to `enabledModels`. The scope does not choose the initial model, as upstream. Each read is a new copy, where upstream returns the stored array. A value that is not an array of `{ model, thinkingLevel? }` with a valid level throws an `Error` and keeps the scope; upstream stores it unchecked. |

## Extension factories, packages, and runtime helpers

- `createCodemodeExtension({ mode, inlineBudget, models })` and `createToolSearchExtension()` return factories that register a `codemode` or `tool_search` tool, inactive, with the definition of the native tool (`js/builtin_extensions.js`, `agent_session/builtins/factories.rs`). Its `execute` runs the native tool of the session of its `ctx`; nested calls of a script go through the context of the calling tool call, also when another tool calls the captured `execute`, as pi-mcp-adapter does. The definition carries `_piNative`, so Rust runs `prepareLoadout` and `reachesTools` natively unless the extension sets its own. Changes to other fields, such as more `promptGuidelines`, renderers, or `exposure`, apply as for any tool. As a regular extension, the tool replaces the built-in `codemode` or `tool_search`, with upstream's warning.
- `createMcpExtension({ logPath, startupWaitMs })` starts the native MCP support of the session on `session_start`, also when `builtin:mcp` is off. It registers no `/mcp` command and no tools through `pi`: the MCP tools register natively, so a proxy over `pi.registerTool` does not see them. The options `loadConfig`, `createTransport`, `credentials`, `openUrl`, and `updateConfig` throw `ERR_PI_UNSUPPORTED`. SDK child sessions emit no `session_start` (`bindExtensions` is unsupported), so there the factory starts MCP only as a `builtin: true` entry (below).
- `DefaultResourceLoader` accepts `extensionFactories` entries with `builtin: true`. Each is a `builtin:<name>` extension resource: on by default, off with `-builtin:<name>` in the `extensions` setting, `disabledBuiltinExtensions`, or `noExtensions`. An enabled entry runs its factory as extension `builtin:<name>`, hidden, before the other factories. A listed `builtin:mcp` starts native MCP when the session is created. `builtin:` paths in `additionalExtensionPaths` still throw `ERR_PI_UNSUPPORTED`.
- `new DefaultPackageManager({ cwd, agentDir, settingsManager, builtinExtensions })` runs each method on the Rust package manager (`extensions/ops_sdk_packages.rs`): `resolve` (with `onMissing`), `resolveExtensionSources`, `install`, `installAndPersist`, `remove`, `removeAndPersist`, `update`, `checkForAvailableUpdates`, `listConfiguredPackages`, `getInstalledPath`, `addSourceToSettings`, `removeSourceFromSettings`, `setProgressCallback`, and the private `parseSource`. The settings manager must be a pi-rust `SettingsManager`; `commandOutput` is ignored. `parseSource` also returns the pi-rust source types `url` and `marketplace`.
- `createExtensionRuntime`, `readStoredCredential`, and `resolveCliModel` are upstream code. `resolveCliModel` resolves in Rust over a pi-rust `ModelRuntime`; another runtime object throws `ERR_PI_UNSUPPORTED`.

These members still throw `ERR_PI_UNSUPPORTED`:

- `agent`, which needs a JavaScript `Agent` backed by Rust.
- `sendCustomMessage`, `sendUserMessage`, `executeBash` (needs `onChunk` and `operations` callbacks), `recordBashResult`, `navigateTree`, `reload`, `cacheWarmingStatus`.
- `promptTemplates`, `resourceLoader`, `bindExtensions`, `extensionRunner`, `hasExtensionHandlers`, `createReplacedSessionContext`, `getToolDefinition`, `summarizeForBugReport`.
- The `createAgentSession` option `sessionStartEvent`, and `extensionsResult.runtime`.
