# Subscription usage limits

Created: 2026-10-30. Last updated: 2026-10-31.

`/usage`, `pi usage`, and the native extension method `usage.get` show the limits of the subscription logins of pi. The code is in `crates/pi-ai/src/usage/` and `crates/pi-ai/src/providers/antigravity_acp/usage.rs`. The tests use local mock servers and an in-process ACP agent. No test calls a live service.

## Sources

Read on 2026-10-30:

- oh-my-pi at commit `5d30c790b883ad0ae05dac2a4d2ccc8ecd103611`: `packages/ai/src/usage/{claude,openai-codex,cursor,google-antigravity,xai-oauth}.ts` and `packages/ai/src/registry/oauth/xai-oauth.ts` (`getXAICliBillingHeaders`, `buildXAICliBillingUrl`).
- paseo at commit `a4f696620b5d248598a882b1a78d82ec5038c74b`: `plugins/{claude,codex,cursor,grok}-usage-source/server/usage.ts`.
- `extensions/subscription-router/src/usage/` in `yannelli/pi-rust-extensions`.
- The Antigravity ACP server 1.4.3 (`agy_acp_server.par`): its `_handle_usage_command` and `usage_lib.format_usage_message`.

## Endpoints

| Provider ID | Request | Auth | Fields that pi reads |
| --- | --- | --- | --- |
| `anthropic` | `GET https://api.anthropic.com/api/oauth/usage` | `Authorization: Bearer <OAuth token>`, `anthropic-beta: oauth-2025-04-20`, `user-agent: claude-cli/<version>`, `x-app: cli` | `five_hour` and `seven_day`: `utilization` (0 to 100) and `resets_at` (ISO 8601). `seven_day_<model>` keys and `limits[]` entries with `kind: "weekly_scoped"` (`percent`, `resets_at`, `scope.model.display_name`) are weekly limits of one model. |
| `openai-codex` | `GET https://chatgpt.com/backend-api/wham/usage` | `Authorization: Bearer <OAuth token>`, `chatgpt-account-id` (stored `accountId`, or the claim of the token), `originator: pi` | `plan_type`. `rate_limit.primary_window` and `secondary_window`: `used_percent`, `limit_window_seconds` (18000 is 5 hours, 604800 is a week), `reset_at` (Unix seconds). `additional_rate_limits[]` has the same shape under `limit_name`. A body that starts with `<` is an HTML error page. |
| `cursor` | `POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage` with body `{}` | `Authorization: Bearer <Cursor login token>`, `connect-protocol-version: 1`, `x-cursor-client-version` | `planUsage.totalPercentUsed`, `autoPercentUsed`, `apiPercentUsed`. `billingCycleStart` and `billingCycleEnd` are Unix milliseconds, as numbers or strings. |
| `xai` | `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits`. For unified billing, also `GET .../v1/billing` without `format`. | `Authorization: Bearer <xai OAuth token>`, `X-XAI-Token-Auth: xai-grok-cli` | `config.currentPeriod` (`start`, `end`, a `type` with `WEEK`), `creditUsagePercent` (missing in an active period means 0), and `productUsage[]` (`product`, `usagePercent`). Unified billing (`isUnifiedBillingUser`, or no weekly data): `billingPeriodStart`, `billingPeriodEnd`, `monthlyLimit.val`, and `used.val`. |
| `google-antigravity` | ACP `session/prompt` with the text `/usage` to the Antigravity ACP server | The server uses its own Google credential. pi holds no Google token. | The Markdown report of the server. pi reads the lines under `### Quota Limits`: `- **<group>**`, then `  - **<bucket>:** 91% remaining (resets Oct 02, 2026, 08:22 UTC)`. |

The server answers `/usage` itself and does not start a model turn. For a personal Google login it calls `cloudcode-pa` `v1internal:retrieveUserQuotaSummary`. For a business login it calls Vertex AI `FetchQuotaStatus`. oh-my-pi calls `retrieveUserQuotaSummary` and `fetchAvailableModels` directly, because it keeps the Google token itself.

oh-my-pi reads Cursor usage from `https://cursor.com/api/usage-summary` with a session cookie, and from `GetPlanInfo` for the weekly reset. pi uses the paseo request, because it uses the bearer token of the Cursor login on the provider host.

## What pi shows

| Provider ID | Windows |
| --- | --- |
| `anthropic` | `5h` (300 minutes), `Weekly` (10080 minutes), and `Weekly <model>` for each model limit, for example `Weekly Opus` |
| `openai-codex` | `5h` and `Weekly` from the window length, or `Primary` and `Secondary` when the response has no length. Extra limits are `<limit name> 5h`. The plan type is the plan. |
| `cursor` | `Total`, `Auto`, and `API` for the billing cycle. The reset is the end of the cycle. |
| `xai` | `Weekly` (the SuperGrok credits) and `Weekly <product>`, for example `Weekly Grok Build` and `Weekly API`. Unified billing shows `Monthly` as `used` of `monthlyLimit`. |
| `google-antigravity` | `<group>: <bucket>`, for example `Gemini Models: Weekly`. The used percentage is 100 minus the remaining percentage. |

- Each window has `label`, `usedPercent` (0 to 100), `resetsAt` (ISO 8601 UTC or `null`), and `windowMinutes` (or `null`). It also has:
  - `scope`: `provider`, or `model` for a limit of some models only, with `families` (for example `["opus"]` from `seven_day_opus`). Every window has `scope`, so a missing `scope` means an older pi.
  - `id`: the name the provider gives the window, for example `five_hour`, `seven_day_opus`, `primary_window`, or `Spark/primary_window` for an extra Codex limit.
  - `resetsAtMs`: the reset in Unix milliseconds, as exact as the provider reports it. Codex `reset_after_seconds` counts from the fetch.
- An entry can also have `credits` (`{ available, unlimited, balance }`, Codex), and a failed entry has `errorKind` (`login`, `unsupported`, `rejected`, `rateLimited`, `http`, `network`, or `response`) and `retryAfterMs` (the `retry-after` header in milliseconds, from a delay or an HTTP date). `plan` is the Codex `plan_type` (or `tier`) and the Anthropic `plan_type`.
- An extension provider that logs in with `oauth: { builtin: "anthropic" | "openai-codex" | ... }` reads its usage with that provider's endpoint and its own login: `usage.get { provider: "anthropic-2" }` sends the token of `anthropic-2`. Its result is cached under its own id; unregistering the provider removes it.
- pi keeps each result for 60 seconds. For `google-antigravity` it keeps a result for 5 minutes, because each fetch starts the ACP server. `/usage` and `pi usage` fetch again. `usage.get` fetches again with `refresh: true`.
- A provider without a login, a rejected token (HTTP 401 or 403), HTTP 429, an HTML page, or an Antigravity report without quota data gives an entry with `error` and no windows.
- pi refreshes an expiring OAuth token under the store lock before it sends the request, as for a model request.

## Not supported

- A separate `grok-build` provider. Pull request 126 removed it because the `xai` login is the same SuperGrok device login. Grok Build use is the `Weekly Grok Build` window of `xai`.
- Usage of API keys. The endpoints above report subscription limits only.
- The `openai` login (Sign in with ChatGPT for the OpenAI API). Its token is for `api.openai.com`, and no source reads `wham/usage` with it.
