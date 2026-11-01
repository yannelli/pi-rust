# SDK child session controls

Created: 2026-11-01. Last updated: 2026-11-01.

This file records which members of the SDK `AgentSession` that `createAgentSession` returns run in pi-rust, compared with upstream `packages/coding-agent/src/core/agent-session.ts` at `35dfa757378bd7a8570670e99c2687bdd1039a06`.

## How it works

- Each member calls the matching native `AgentSession` method through `sdk.read`, `sdk.control`, or an async `sdk.<name>` op (`crates/pi-coding-agent/src/extensions/ops_sdk/child.rs`). `js/sdk_session.js` passes arguments and results only.
- `subscribe` stays active until its unsubscribe function, the last unsubscribe, or `dispose`. Events outside a prompt reach the listeners. Rust keeps one session listener per child and one task that calls the JavaScript callback in order. Unsubscribing the last listener, `dispose`, and the shutdown of the extension runner remove the listener and stop the task.
- `prompt` resolves after its events reach the listeners. When a listener throws, the run is aborted and `prompt` rejects with that error.
- `setModel`, `cycleModel`, `setThinkingLevel`, and `cycleThinkingLevel` save to settings only with `{ persist: true }`, as upstream. The `ctx` actions of extensions keep their own defaults.

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

These members still throw `ERR_PI_UNSUPPORTED`:

- `agent`, which needs a JavaScript `Agent` backed by Rust.
- `sendCustomMessage`, `sendUserMessage`, `executeBash` (needs `onChunk` and `operations` callbacks), `recordBashResult`, `navigateTree`, `reload`, `cacheWarmingStatus`.
- `scopedModels`, `setScopedModels`, `promptTemplates`, `resourceLoader`, `bindExtensions`, `extensionRunner`, `hasExtensionHandlers`, `createReplacedSessionContext`, `getToolDefinition`, `summarizeForBugReport`.
- The `createAgentSession` options `customTools`, `scopedModels`, and `sessionStartEvent`, and `extensionsResult.runtime`.
