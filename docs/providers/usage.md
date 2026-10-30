# Subscription usage limits

Created: 2026-10-30. Last updated: 2026-10-30.

`/usage`, `pi usage`, and the native extension method `usage.get` show the limits of the subscription logins of pi. The code is in `crates/pi-ai/src/usage/` and `crates/pi-ai/src/providers/antigravity_acp/usage.rs`. The tests use local mock servers and an in-process ACP agent. No test calls a live service.

## Sources

Read on 2026-10-30:

- oh-my-pi at commit `5d30c790b883ad0ae05dac2a4d2ccc8ecd103611`: `packages/ai/src/usage/{claude,openai-codex,cursor,google-antigravity,xai-oauth}.ts`.
- paseo at commit `a4f696620b5d248598a882b1a78d82ec5038c74b`: `plugins/{claude,codex,cursor,grok}-usage-source/server/usage.ts`.
- `extensions/subscription-router/src/usage/` in `yannelli/pi-rust-extensions`.
- The Antigravity ACP server 1.4.3 (`agy_acp_server.par`): its `_handle_usage_command` and `usage_lib.format_usage_message`.

## Endpoints

| Provider ID | Request | Auth | Fields that pi reads |
| --- | --- | --- | --- |
| `anthropic` | `GET https://api.anthropic.com/api/oauth/usage` | `Authorization: Bearer <OAuth token>`, `anthropic-beta: oauth-2025-04-20`, `user-agent: claude-cli/<version>`, `x-app: cli` | `five_hour` and `seven_day`: `utilization` (0 to 100) and `resets_at` (ISO 8601). `seven_day_<model>` keys and `limits[]` entries with `kind: "weekly_scoped"` (`percent`, `resets_at`, `scope.model.display_name`) are weekly limits of one model. |
| `openai-codex` | `GET https://chatgpt.com/backend-api/wham/usage` | `Authorization: Bearer <OAuth token>`, `chatgpt-account-id` (stored `accountId`, or the claim of the token), `originator: pi` | `plan_type`. `rate_limit.primary_window` and `secondary_window`: `used_percent`, `limit_window_seconds` (18000 is 5 hours, 604800 is a week), `reset_at` (Unix seconds). `additional_rate_limits[]` has the same shape under `limit_name`. A body that starts with `<` is an HTML error page. |
| `cursor` | `POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage` with body `{}` | `Authorization: Bearer <Cursor login token>`, `connect-protocol-version: 1`, `x-cursor-client-version` | `planUsage.totalPercentUsed`, `autoPercentUsed`, `apiPercentUsed`. `billingCycleStart` and `billingCycleEnd` are Unix milliseconds, as numbers or strings. |
| `google-antigravity` | ACP `session/prompt` with the text `/usage` to the Antigravity ACP server | The server uses its own Google credential. pi holds no Google token. | The Markdown report of the server. pi reads the lines under `### Quota Limits`: `- **<group>**`, then `  - **<bucket>:** 91% remaining (resets Oct 02, 2026, 08:22 UTC)`. |

The server answers `/usage` itself and does not start a model turn. For a personal Google login it calls `cloudcode-pa` `v1internal:retrieveUserQuotaSummary`. For a business login it calls Vertex AI `FetchQuotaStatus`. oh-my-pi calls `retrieveUserQuotaSummary` and `fetchAvailableModels` directly, because it keeps the Google token itself.

oh-my-pi reads Cursor usage from `https://cursor.com/api/usage-summary` with a session cookie, and from `GetPlanInfo` for the weekly reset. pi uses the paseo request, because it uses the bearer token of the Cursor login on the provider host.

## What pi shows

| Provider ID | Windows |
| --- | --- |
| `anthropic` | `5h` (300 minutes), `Weekly` (10080 minutes), and `Weekly <model>` for each model limit, for example `Weekly Opus` |
| `openai-codex` | `5h` and `Weekly` from the window length, or `Primary` and `Secondary` when the response has no length. Extra limits are `<limit name> 5h`. The plan type is the plan. |
| `cursor` | `Total`, `Auto`, and `API` for the billing cycle. The reset is the end of the cycle. |
| `google-antigravity` | `<group>: <bucket>`, for example `Gemini Models: Weekly`. The used percentage is 100 minus the remaining percentage. |

- Each window has `label`, `usedPercent` (0 to 100), `resetsAt` (ISO 8601 UTC or `null`), and `windowMinutes` (or `null`).
- pi keeps each result for 60 seconds. For `google-antigravity` it keeps a result for 5 minutes, because each fetch starts the ACP server. `/usage` and `pi usage` fetch again. `usage.get` fetches again with `refresh: true`.
- A provider without a login, a rejected token (HTTP 401 or 403), HTTP 429, an HTML page, or an Antigravity report without quota data gives an entry with `error` and no windows.
- pi refreshes an expiring OAuth token under the store lock before it sends the request, as for a model request.

## Not supported

- Grok Build: pull request 126 removed the `grok-build` provider because the `xai` login is the same SuperGrok device login. paseo and oh-my-pi read Grok limits from `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` with `Authorization: Bearer <token>` and `X-XAI-Token-Auth: xai-grok-cli` (`config.creditUsagePercent`, `config.currentPeriod.end`). oh-my-pi sends the token of the `xai` OAuth login to this endpoint. pi does not have a fetcher for `xai` yet.
- Usage of API keys. The endpoints above report subscription limits only.
- The `openai` login (Sign in with ChatGPT for the OpenAI API). Its token is for `api.openai.com`, and no source reads `wham/usage` with it.
