# Subscription providers: Cursor, Grok Build, and Google Antigravity

Created: 2026-10-29. Last updated: 2026-10-30.

These three providers are pi-rust extensions. Upstream pi does not have them. Their models are in `crates/pi-ai/models.extra.json`. `cargo xtask models` regenerates `models.json` from the upstream catalog and does not change this file.

| Provider ID | API | Login | Code |
| --- | --- | --- | --- |
| `cursor` | `cursor-agent` | Cursor CLI browser login, then pi polls for the result | `providers/cursor/`, `auth/oauth/cursor.rs` |
| `grok-build` | `grok-build-responses` | xAI device code | `providers/grok_build.rs`, `auth/oauth/xai.rs` |
| `google-antigravity` | `antigravity-acp` | Google login of the installed Antigravity ACP server | `providers/antigravity_acp/`, `auth/oauth/antigravity_acp.rs` |

All paths are in `crates/pi-ai/src`. The tests use local mock servers. No test calls a live service.

## Cursor

References: [oh-my-pi `cursor.ts`](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/providers/cursor.ts), [`agent.proto`](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/providers/cursor/proto/agent.proto), [oh-my-pi Cursor OAuth](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/registry/oauth/cursor.ts), and River `RiverCloud/src/cursor` (login UX).

Login:

1. pi makes a PKCE pair and a UUID, then opens `https://cursor.com/loginDeepControl?challenge=…&uuid=…&mode=login&redirectTarget=cli`.
2. pi polls `GET https://api2.cursor.sh/auth/poll?uuid=…&verifier=…`. HTTP 404 means the login is not complete yet. The poll delay starts at 1 s and grows by 1.2 times to a maximum of 10 s. Three errors in a row stop the login.
3. The JWT `exp` claim minus 5 minutes sets the token expiry.

Refresh: `POST https://api2.cursor.sh/oauth/token` with the client ID `KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB`. A reply with `shouldLogout: true` ends the session. Then the user must run `/login cursor` again.

Chat goes over HTTP/1.1. reqwest does not need its `http2` feature.

- `POST /agent.v1.AgentService/RunSSE` sends one Connect frame that holds the request ID. The reply streams Connect frames: 1 flag byte, a 4-byte big-endian length, then the payload. Flag `0x02` marks the end-of-stream JSON.
- Each client message is one `POST /aiserver.v1.BidiService/BidiAppend`. Its body is an unframed `BidiAppendRequest{request_id, seqno, data}`. The run request goes first. A heartbeat goes every 5 s.
- Each call rebuilds the transcript as blobs. The key of each blob is its SHA-256 hash. The blobs are JSON prompt records (`root_prompt_messages_json`) and structured turns. When the request has history, a `<rules>` record carries the system prompt. pi limits tool call IDs to `[A-Za-z0-9_-]` and 64 characters.
- The pi tools go to Cursor as MCP tools of the `pi-agent` server, in the `requestContext` reply. pi answers each `mcp_args` exec with a handoff text, and the turn ends with stop reason `toolUse`. The next call sends a `ResumeAction` with the tool results in the history.
- pi answers `getBlobArgs`, `setBlobArgs`, `mcp_state`, hook, and interaction queries. It rejects other exec variants with `exec_variant_unsupported`.
- Requests send `x-cursor-client-version: cli-2026.09.02-c22c1a3`. No environment variable replaces it. To send a different version, set the header in the `headers` of the `cursor` provider in `models.json`.
- Model list: after the login, pi sends `POST /aiserver.v1.AiService/AvailableModels` with `AvailableModelsRequest{include_long_context_models, use_model_parameters, do_not_use_markdown, use_cloud_agent_effort_modes}`. Each variant becomes one model:
  - The model ID is the `legacy_slug` of the variant. A model without variants uses its `name`.
  - The context size is `context_token_limit`. A max mode variant uses `context_token_limit_for_max_mode`.
  - `compat.cursorModel`, `compat.cursorParameters`, and `compat.cursorMaxMode` keep the route. pi sends them in `RequestedModel`.
  - pi does not show hidden models (`is_hidden`).
- Context use: each `conversation_checkpoint_update` carries `token_details.used_tokens`. pi puts the last value in `usage.totalTokens`, which the footer shows as context use.
- Model IDs without a `compat` route: a GPT ID with an effort suffix asks for its base model with a `reasoning` parameter, for example `gpt-5.5-high` gives `gpt-5.5` with `reasoning=high`. `composer-2.5` sends `fast=false`. pi sends other IDs unchanged.

## Grok Build

References: Nextvisit/platform `GrokBuildProvider`, `GrokBuildRequestBody`, and `GrokRequestHeaders`; River `subscriptions/grok.ts`.

- Login is the xAI device-code flow of the `xai` provider: client `b1a00492-073a-47ea-816f-4c329264a828`, with the scopes `grok-cli:access api:access`.
- Requests go to `https://cli-chat-proxy.grok.com/v1/responses` with the Grok CLI headers: `X-XAI-Token-Auth: xai-grok-cli`, `x-grok-client-version`, `x-grok-client-identifier: grok-shell`, `x-grok-client-mode: cli`, `x-grok-model-override`, and `x-grok-conv-id`. The default client version is `1.0.45`. `GROK_CLIENT_VERSION` replaces it.
- pi changes the OpenAI Responses body for the proxy:
  - pi moves system and developer items into `instructions`.
  - pi removes item IDs, and reasoning items that have no `encrypted_content`.
  - pi removes empty text parts.
  - pi removes `temperature`, `top_p`, `max_output_tokens`, `prompt_cache_retention`, and `user`.
  - pi sends tools with `strict: false` and an object schema.
  - `reasoning` keeps only `effort`. When thinking is off, pi sends no `reasoning`.
- HTTP 426 means the client version is too old. HTTP 402 and 403 mean the plan does not include Grok Build, or no usage is left.

## Google Antigravity

References: [pi-antigravity-acp-provider](https://www.npmjs.com/package/pi-antigravity-acp-provider) and [omp-antigravity-acp-provider](https://www.npmjs.com/package/omp-antigravity-acp-provider).

pi is an Agent Client Protocol (ACP) client of Google's Antigravity ACP server. pi does not keep a Google client ID, a secret, or a Google token.

- Install: if pi does not find the server, `/login google-antigravity` asks to install it. Then pi:
  1. Downloads Google's release 1.4.3 for the platform from `dl.google.com`.
  2. Checks the size and the SHA-256 of the archive, and the two files in it.
  3. Unpacks it with the system `unzip` (or `tar`) into `~/.local/opt/agy-acp/versions/<sha256>/`.
  4. Points `~/.local/opt/agy-acp/current` at that directory.

  You can also install `agy_acp_server.par` (`agy_acp_server.exe` on Windows) with `localharness_external` in the same directory yourself. pi looks for the server in this order:
  1. `PI_ANTIGRAVITY_ACP_SERVER`
  2. `AGY_ACP_BIN`
  3. `~/.local/opt/agy-acp/current/`
  4. `~/.local/bin/`
  5. `PATH`
- Process: pi starts the server in the working directory and sends JSON-RPC 2.0 messages as newline-delimited JSON on stdin and stdout. pi adds the loopback addresses to `NO_PROXY`. On Linux, pi gives `--uid=` to a server file that does not start with `#!`.
- Login: `/login google-antigravity` sends `authenticate` with the Google method of the server (`oauth-personal`). The server opens the browser and keeps the credential. pi stores only the marker `antigravity-acp` in `auth.json`. If `GEMINI_API_KEY` is set, pi does not send `authenticate`.
- Session: pi keeps one ACP session for each pi session. `session/new` gets:
  - the `pi-bridge` MCP server of pi;
  - the MCP servers of the user that the agent can reach itself;
  - `_meta.antigravity` when the server shows `systemPrompt.append`. This holds the system prompt, `eagerMcpServers: ["pi-bridge"]`, and `enabledTools: []`, which turns off the built-in tools of Antigravity.
- Model and thinking: pi sets the `model` selector to the model ID and the `thought_level` selector to the thinking level of the model row, with `session/set_config_option`. An older server gets `session/set_model`. If the server does not offer the model, the error lists the models that it offers.
- Tools: the bridge is a streamable HTTP MCP server on `127.0.0.1`. It accepts only `POST /mcp` with a random bearer token. It shows each pi tool as `pi_<name>`. A `tools/call` stays open while pi runs the tool, with an SSE keepalive every 15 s:
  1. The pi stream ends with stop reason `toolUse`.
  2. The next pi call sends the tool result to the open call.
  3. The same `session/prompt` continues.
- User MCP servers: pi sends the enabled stdio servers, and the HTTP servers that do not use a login that pi holds, to the agent in `session/new`. pi resolves `${VAR}`, `!command`, and `~` as for its own MCP connections. The bridge then does not show pi's copies of their tools (`mcp__<server>__*`).
- Prompt: a new session gets one `resource` block with the system prompt (when `_meta` cannot carry it) and the prior conversation, then the user request. A session that continues gets only the messages that it did not see.
- Updates: `agent_message_chunk` becomes text, and `agent_thought_chunk` becomes thinking. `usage_update.used` becomes `usage.totalTokens`, which pi shows as context use. pi selects the first `allow_once` option of a `session/request_permission` request. pi answers other agent requests with `-32601`.

## Not done yet

- Account model lists: pi does not read Grok Build `GET /models` yet. Antigravity shows the bundled model rows; the server rejects a model that it does not offer.
- Antigravity: pi does not resume a server session after pi restarts (`session/load`). A new pi process starts a new ACP session with the conversation in the context resource.
- Cursor: pi does not keep a run open between calls over HTTP/2. pi does not change to a new conversation ID after a `resource_exhausted` error.
