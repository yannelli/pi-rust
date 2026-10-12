//! Cloudflare Workers AI and AI Gateway helpers: base URL templates filled from the
//! provider env, and gateway auth headers. Port of upstream `api/cloudflare.ts`,
//! `providers/cloudflare-stream.ts`, and `providers/cloudflare-auth.ts`.

use std::borrow::Cow;

use crate::providers::provider_env::provider_env_value;
use crate::registry::StreamOptions;
use crate::types::Model;

/// Workers AI OpenAI-compatible endpoint.
pub const CLOUDFLARE_WORKERS_AI_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1";
/// Workers AI REST root; model runs are posted to `run`.
pub const CLOUDFLARE_WORKERS_AI_REST_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai";
/// AI Gateway unified (OpenAI-compatible) API.
pub const CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat";
/// AI Gateway passthrough to OpenAI, used for the Responses API.
pub const CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai";
/// AI Gateway passthrough to Anthropic.
pub const CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic";

pub const CLOUDFLARE_WORKERS_AI_PROVIDER: &str = "cloudflare-workers-ai";
pub const CLOUDFLARE_AI_GATEWAY_PROVIDER: &str = "cloudflare-ai-gateway";
pub const CLOUDFLARE_API_KEY: &str = "CLOUDFLARE_API_KEY";
pub const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
pub const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

/// Fill `{CLOUDFLARE_ACCOUNT_ID}` and `{CLOUDFLARE_GATEWAY_ID}` from `options.env`, then the
/// process environment. Unknown values keep their placeholder, as upstream does.
pub fn resolve_cloudflare_base_url(base_url: &str, env: &std::collections::BTreeMap<String, String>) -> String {
    let mut resolved = base_url.to_string();
    for name in [CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_GATEWAY_ID] {
        let placeholder = format!("{{{name}}}");
        if resolved.contains(&placeholder)
            && let Some(value) = provider_env_value(name, env)
        {
            resolved = resolved.replace(&placeholder, &value);
        }
    }
    resolved
}

/// The model with its Cloudflare base URL placeholders filled; unchanged models are borrowed.
pub fn resolve_cloudflare_model<'a>(model: &'a Model, options: &StreamOptions) -> Cow<'a, Model> {
    let base_url = resolve_cloudflare_base_url(&model.base_url, &options.env);
    if base_url == model.base_url {
        return Cow::Borrowed(model);
    }
    Cow::Owned(Model { base_url, ..model.clone() })
}

/// AI Gateway auth: the Cloudflare key goes in `cf-aig-authorization`, and the provider
/// `Authorization` and `x-api-key` defaults are suppressed.
pub fn apply_ai_gateway_auth(options: &mut StreamOptions, api_key: &str) {
    options.headers.insert("cf-aig-authorization".into(), format!("Bearer {api_key}"));
    for name in ["authorization", "x-api-key"] {
        options.headers.retain(|header, _| !header.eq_ignore_ascii_case(name));
        if !options.suppressed_headers.iter().any(|header| header.eq_ignore_ascii_case(name)) {
            options.suppressed_headers.push(name.into());
        }
    }
}

/// Prepare a request for a Cloudflare model: template the base URL, fall back to
/// `CLOUDFLARE_API_KEY`, and move AI Gateway keys into gateway auth. Other models pass through.
pub fn prepare_cloudflare_request<'a>(model: &'a Model, mut options: StreamOptions) -> (Cow<'a, Model>, StreamOptions) {
    let model = resolve_cloudflare_model(model, &options);
    let provider = model.provider.as_str();
    if provider != CLOUDFLARE_WORKERS_AI_PROVIDER && provider != CLOUDFLARE_AI_GATEWAY_PROVIDER {
        return (model, options);
    }
    if options.api_key.as_deref().is_none_or(str::is_empty) {
        options.api_key = provider_env_value(CLOUDFLARE_API_KEY, &options.env);
    }
    let gateway_auth_set = options.headers.keys().any(|name| name.eq_ignore_ascii_case("cf-aig-authorization"));
    if provider == CLOUDFLARE_AI_GATEWAY_PROVIDER
        && !gateway_auth_set
        && let Some(key) = options.api_key.take()
    {
        apply_ai_gateway_auth(&mut options, &key);
    }
    (model, options)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn model(provider: &str, base_url: &str) -> Model {
        Model { id: "m".into(), provider: provider.into(), base_url: base_url.into(), ..Model::default() }
    }

    fn env() -> BTreeMap<String, String> {
        BTreeMap::from([
            (CLOUDFLARE_ACCOUNT_ID.to_string(), "acct".to_string()),
            (CLOUDFLARE_GATEWAY_ID.to_string(), "gw".to_string()),
        ])
    }

    #[test]
    fn fills_account_and_gateway_placeholders() {
        assert_eq!(
            resolve_cloudflare_base_url(CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL, &env()),
            "https://gateway.ai.cloudflare.com/v1/acct/gw/compat"
        );
        assert_eq!(
            resolve_cloudflare_base_url(CLOUDFLARE_WORKERS_AI_BASE_URL, &env()),
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1"
        );
        let options = StreamOptions::default();
        let plain = model("openai", "https://api.openai.com/v1");
        assert!(matches!(resolve_cloudflare_model(&plain, &options), Cow::Borrowed(_)));
    }

    #[test]
    fn workers_ai_keeps_bearer_keys_and_gateway_moves_them() {
        let options = StreamOptions { env: env(), api_key: Some("cf-key".into()), ..StreamOptions::default() };
        let workers = model(CLOUDFLARE_WORKERS_AI_PROVIDER, CLOUDFLARE_WORKERS_AI_BASE_URL);
        let (resolved, prepared) = prepare_cloudflare_request(&workers, options.clone());
        assert_eq!(resolved.base_url, "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1");
        assert_eq!(prepared.api_key.as_deref(), Some("cf-key"));

        let gateway = model(CLOUDFLARE_AI_GATEWAY_PROVIDER, CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL);
        let mut with_auth = options;
        with_auth.headers.insert("Authorization".into(), "Bearer other".into());
        let (resolved, prepared) = prepare_cloudflare_request(&gateway, with_auth);
        assert_eq!(resolved.base_url, "https://gateway.ai.cloudflare.com/v1/acct/gw/anthropic");
        assert_eq!(prepared.api_key, None);
        assert_eq!(prepared.headers.get("cf-aig-authorization").map(String::as_str), Some("Bearer cf-key"));
        assert!(!prepared.headers.contains_key("Authorization"));
        assert_eq!(prepared.suppressed_headers, ["authorization", "x-api-key"]);
    }

    #[tokio::test]
    async fn registry_requests_use_templated_gateway_urls_and_auth() {
        use crate::providers::mock_server::{MockResponse, MockServer, frame};
        let done =
            frame(None, &serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]}));
        let mut server = MockServer::start(MockResponse::Sse(vec![done])).await;
        let base_url = format!("{}/v1/{{CLOUDFLARE_ACCOUNT_ID}}/{{CLOUDFLARE_GATEWAY_ID}}/compat", server.base_url);
        let gateway = Model { api: "openai-completions".into(), ..model(CLOUDFLARE_AI_GATEWAY_PROVIDER, &base_url) };
        let options = StreamOptions { env: env(), api_key: Some("cf-key".into()), ..StreamOptions::default() };
        let context = crate::types::Context { messages: vec![crate::types::Message::user("hi")], ..Default::default() };
        let message = crate::registry::complete(&gateway, &context, options).await;
        assert_eq!(message.text(), "ok", "{:?}", message.error_message);
        let request = server.request().await;
        assert_eq!(request.path, "/v1/acct/gw/compat/chat/completions");
        assert_eq!(request.headers["cf-aig-authorization"], "Bearer cf-key");
        assert!(!request.headers.contains_key("authorization"));
    }
}
