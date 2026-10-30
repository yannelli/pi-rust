# Providers

## Protocols and logins

pi-rust has all the wire protocols of upstream pi:

- `anthropic-messages`
- `openai-responses`, `openai-completions`, `openai-codex-responses` (SSE and WebSocket), and `azure-openai-responses`
- `google-generative-ai` and `google-vertex`
- `bedrock-converse-stream`, `mistral-conversations`, and `pi-messages`

The bundled model catalog is the catalog of upstream pi. `pi update --models` updates it.

pi-rust also has `cursor-agent` and `antigravity-acp` for the subscription providers in the next section.

`/login` supports OAuth for Anthropic, OpenAI Codex, GitHub Copilot, OpenRouter, xAI, Kimi, Meta, Radius, Cursor, and Google Antigravity. pi also accepts API keys and the default AWS and Google credentials of your system. llama.cpp servers and virtual models work as in upstream pi.

## Cursor and Google Antigravity (pi-rust extension)

These providers use your subscription. Log in with `/login cursor` or `/login google-antigravity`.

| Provider | Login | Default model |
| --- | --- | --- |
| `cursor` | pi opens the Cursor login page and waits until you approve the login. | `claude-opus-5` |
| `google-antigravity` | If Google's Antigravity ACP server is not installed, pi asks to install it. Then open the Google sign-in link that pi shows. On a remote machine, paste the `127.0.0.1` address from the browser into pi. | `gemini-3.1-pro` |

- Cursor runs the model on Cursor's agent service. pi gives its tools to Cursor as MCP tools and runs them itself. The model list, the thinking levels, and the context sizes come from your Cursor account.
- Google Antigravity runs through the Antigravity ACP server. The agent uses the pi tools, so pi runs each tool. The agent also connects to your MCP servers. The model list and the thinking levels come from the server.

[Subscription providers](../../../docs/providers/subscription-providers.md) has the protocol details.

## Fast mode (pi-rust extension)

Some models have a fast mode. The provider runs the model faster and charges more. Upstream pi does not have fast mode.

| Provider | Models | Request |
| --- | --- | --- |
| `anthropic` on the Claude API | Claude Opus 5.5, Opus 5, and Opus 4.8 | pi sends `speed: "fast"`. The price is 2 times the standard price. |
| `openai-codex` | Codex models that offer the priority tier | pi sends `service_tier: "priority"`. |
| `cursor` | Models that have fast variants | pi sends the fast variant of the model. |

To turn fast mode on or off:

- In the TUI, type `/fast` to toggle fast mode. Type `/fast on` or `/fast off` to set it. When fast mode is on and the model supports it, the footer shows `⚡ fast` next to the thinking level.
- To turn on fast mode for one run, start pi with `--fast`. pi does not save this choice.
- In ACP mode, use the **Speed** option. The editor shows this option only for models that have a fast mode.
- In RPC mode, send `{"type": "set_fast_mode", "enabled": true}`. `get_state` returns `fastMode` and `fastModeAvailable`.

`/fast` and the ACP **Speed** option save the choice as `fastMode` in the global `settings.json`. New sessions read this setting. RPC `set_fast_mode` does not save it.

Fast mode stays on when you select a model that has no fast mode. pi sends standard requests for that model.

To mark a model in `models.json`, set `compat.fastMode`. `true` gives the model a fast mode, and `false` removes it. pi sends the fast request only for the `anthropic-messages`, `openai-codex-responses`, and `cursor-agent` APIs.

```json
{
  "providers": {
    "my-gateway": {
      "api": "anthropic-messages",
      "models": [{ "id": "my-model", "compat": { "fastMode": true } }]
    }
  }
}
```

## Disable providers

To hide a provider from model selection, add its exact ID to `disabledProviders` in `~/.pi/agent/settings.json`. If you set `PI_CODING_AGENT_DIR`, use the `settings.json` in that directory.

```json
{
  "disabledProviders": ["openrouter", "google-antigravity"]
}
```

You can also open `/settings` in the TUI and use **Disabled providers**. Press Enter to switch the selected provider between enabled and disabled. Type to search the list; Space also switches the provider when the search box is empty. The change applies immediately and pi saves it to the global settings file.

After you edit the file manually, restart pi or its ACP connection.

- pi removes the models of these providers from model selection. This applies to ACP, RPC, the TUI, CLI lists, and completions.
- The filter stays after a catalog refresh. It also applies to custom models and extension models.
- Project settings cannot override the filter.
- pi keeps the credentials and the cached catalogs. To show the models again, remove the ID.

## Retry settings

The TUI `/settings` menu shows these items after **Cache warming**. Each change applies immediately and pi saves it to the global settings file.

| Item | Setting | Default |
| --- | --- | --- |
| Auto-retry | `retry.enabled` | `true` |
| Max retries | `retry.maxRetries` | `3` |
| Retry base delay | `retry.baseDelayMs` | 2 sec |
| Max retry wait | `retry.maxAgentDelayMs` | 1 min |
| Provider request timeout | `retry.provider.timeoutMs` | `default`: `httpIdleTimeoutMs`, or the provider timeout when that is 0 |
| Provider max retries | `retry.provider.maxRetries` | `default`: the provider limit |
| Max server retry delay | `retry.provider.maxRetryDelayMs` | 1 min; `no limit` writes 0 |

When you set **Provider request timeout** or **Provider max retries** to `default`, pi removes the key from `settings.json`. After you edit the file manually, restart pi.

## OpenRouter

To log in, use `/login openrouter` or set `OPENROUTER_API_KEY`.

With a key, pi lists only the OpenRouter chat models that the key can use. For each model, it shows the context size, the maximum output, and the prices for that key. This data comes from the OpenRouter `/models/user` endpoint. Thus it includes the guardrails and provider limits that you set in OpenRouter. pi does not filter image and classifier models.

pi updates the list in the background at these times:

- When it starts in interactive, RPC, or ACP mode.
- When `/model` opens.
- After `/login`.
- When you run `pi update --models`.

pi saves the last list in `models-store.json`. It uses this list offline, in `pi -p`, and until an update finishes. If you change keys outside `/login`, pi keeps the list of the old key until the next update.

Regional endpoints show only the models of their region. To use a regional endpoint, set the base URL in `models.json`:

```json
{
  "providers": {
    "openrouter": { "baseUrl": "https://us.openrouter.ai/api/v1" }
  }
}
```

If the base URL rejects the key at `/models/user`, or does not have that endpoint, pi lists all OpenRouter models. A proxy that you set as the OpenRouter base URL is an example.

## Account model lists

Upstream pi limits only OpenRouter to the models of your key. pi-rust does the same for other providers. With a credential, pi gets the model list of the provider a maximum of 1 time in 4 hours. pi also gets the list immediately after `/login` and when you run `pi update --models`.

When pi gets the list, it does these steps:

1. It removes the catalog chat models that the list does not name.
2. It uses the context size and maximum output from the list, if the list has them.
3. It adds the listed chat models that the catalog does not have. Each one is a copy of the nearest catalog model.

If the list does not name the default model of the provider, pi removes no models. It only uses the limits and adds models.

pi does not filter image and classifier models. pi saves the list in `models-store.json` with the pi version. It does not save the credential. The base URL that you set in `models.json` applies. After an upgrade, pi shows the full catalog until a new list arrives. Model list requests do not follow redirects, so pi sends the credential only to the configured host.

### Full catalog

To get the full catalog, set the `baseUrl` of the provider in `models.json` to an endpoint that has no model list, such as a proxy.

pi also shows all catalog models in these cases:

- The model list answers 401, 403, or 404.
- pi cannot read the body of the model list.
- The list is empty or names no catalog chat model. In this case, pi does not save the list.

After other failures, redirects too, pi keeps the last saved list.

### OpenAI Codex

With a ChatGPT login, pi reads the models of the account from the Codex backend at `<base URL>/codex/models`. This is the list that Codex shows. pi skips hidden models. The endpoint is not a public API, so pi accepts some changes in its format. For example, pi uses `id` when `slug` is missing.

pi sends `client_version=0.166.0`, the Codex CLI release of 2026-10-29. The backend leaves out models that need a newer client. pi copies the `service_tiers[].id` values of each model into `compat.serviceTiers`. A model with `priority` in this list has a [fast mode](#fast-mode-pi-rust-extension).

### Anthropic

With an API key or a Claude subscription login, pi reads `GET /v1/models` with the authentication headers of the provider. It reads all pages of the list. The list gives the context size and maximum output of each model.

### GitHub Copilot

With a Copilot login, pi reads `GET /models` from the Copilot endpoint of the login. It keeps the chat models that support tools, that the model picker shows, and that no policy disables. `/login` selects models with the same rules. The endpoint of the token has priority over the `models.json` base URL, as it does for requests.

### OpenAI and xAI

With an API key, pi reads `GET <base URL>/models`. This list also names embedding, audio, image, moderation, and realtime models. Thus pi adds only listed models of a model family that the catalog has, such as `gpt-` or `grok-`. It does not add models of the other types. The list gives no limits, so pi keeps the limits from the catalog.

With a ChatGPT login for `openai`, the same request returns the models of the account by `slug`. pi uses the `context_window` of each model as its context size. It does not use the larger `max_context_window`.

### Google

With a Gemini API key, pi reads `GET <base URL>/models` with `x-goog-api-key` and reads all pages. It uses only the models that support `generateContent`, with their input and output token limits. pi adds new models only for a model family that the catalog has.

### Cursor

With a Cursor login, pi reads `AvailableModels` from the Cursor agent service. pi shows one model for each Cursor model, for example `grok-4.7`. If Cursor also has fast variants, pi adds a second model, for example `grok-4.7-fast`.

- The effort variants of a model are its thinking levels. `thinking=false` and the effort `none` are the level `off`.
- The context size is the `context` value of the default variant. If the variant has no `context` value, pi uses the limit that Cursor gives for the model.
- pi does not show max mode variants.

### Google Antigravity

With a Google Antigravity login, pi starts the Antigravity ACP server and opens a session. pi reads the `model` selector of the session. For each model, pi reads the values of the `thought_level` selector, which become the thinking levels of the model. The value `extra_high` is the level `xhigh`. The context size comes from the bundled model of the same ID, else from the first bundled Antigravity model.

If the server is not installed, pi shows the bundled models.

### Other providers

Other providers show the full catalog.
