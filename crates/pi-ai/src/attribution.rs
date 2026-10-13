//! Provider attribution and session headers, ported from upstream coding-agent
//! `core/provider-attribution.ts`. Later header sources override earlier ones.

use std::collections::BTreeMap;

use reqwest::Url;

use crate::types::Model;

const OPENROUTER_HOST: &str = "openrouter.ai";
const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";
const CLOUDFLARE_AI_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";
const OPENCODE_HOST: &str = "opencode.ai";

pub type ProviderHeaders = BTreeMap<String, String>;

fn matches_host(base_url: &str, expected: &str) -> bool {
    Url::parse(base_url).ok().and_then(|url| url.host_str().map(|host| host == expected)).unwrap_or(false)
}

fn headers(pairs: &[(&str, &str)]) -> ProviderHeaders {
    pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
}

/// Attribution headers sent when install telemetry is on.
pub fn default_attribution_headers(model: &Model, telemetry_enabled: bool) -> Option<ProviderHeaders> {
    if !telemetry_enabled {
        return None;
    }
    let provider = model.provider.as_str();
    if provider == "openrouter" || model.base_url.contains(OPENROUTER_HOST) {
        return Some(headers(&[
            ("HTTP-Referer", "https://pi.dev"),
            ("X-OpenRouter-Title", "pi"),
            ("X-OpenRouter-Categories", "cli-agent"),
        ]));
    }
    if provider == "nvidia" || matches_host(&model.base_url, NVIDIA_NIM_HOST) {
        return Some(headers(&[("X-BILLING-INVOKE-ORIGIN", "Pi")]));
    }
    let cloudflare = matches!(provider, "cloudflare-workers-ai" | "cloudflare-ai-gateway")
        || matches_host(&model.base_url, CLOUDFLARE_API_HOST)
        || matches_host(&model.base_url, CLOUDFLARE_AI_GATEWAY_HOST);
    cloudflare.then(|| headers(&[("User-Agent", "pi-coding-agent")]))
}

/// OpenCode session headers; other providers get none.
pub fn session_headers(model: &Model, session_id: Option<&str>) -> Option<ProviderHeaders> {
    let session = session_id.filter(|session| !session.is_empty())?;
    let opencode =
        matches!(model.provider.as_str(), "opencode" | "opencode-go") || matches_host(&model.base_url, OPENCODE_HOST);
    opencode.then(|| headers(&[("x-opencode-session", session), ("x-opencode-client", "pi")]))
}

/// Session headers, then attribution headers, then each source in order. `None` when empty.
pub fn merge_provider_attribution_headers(
    model: &Model,
    telemetry_enabled: bool,
    session_id: Option<&str>,
    sources: &[Option<&ProviderHeaders>],
) -> Option<ProviderHeaders> {
    let mut merged = session_headers(model, session_id).unwrap_or_default();
    merged.extend(default_attribution_headers(model, telemetry_enabled).unwrap_or_default());
    for source in sources.iter().flatten() {
        merged.extend(source.iter().map(|(name, value)| (name.clone(), value.clone())));
    }
    (!merged.is_empty()).then_some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock_server::test_model;

    fn model(provider: &str, base_url: &str) -> Model {
        test_model("openai-completions", provider, base_url)
    }

    #[test]
    fn attribution_follows_provider_and_host() {
        let openrouter = default_attribution_headers(&model("openrouter", "https://openrouter.ai/api/v1"), true);
        assert_eq!(openrouter.unwrap()["X-OpenRouter-Categories"], "cli-agent");
        let proxied = default_attribution_headers(&model("custom", "https://proxy.example/openrouter.ai/v1"), true);
        assert_eq!(proxied.unwrap()["HTTP-Referer"], "https://pi.dev");
        let nim = default_attribution_headers(&model("custom", "https://integrate.api.nvidia.com/v1"), true);
        assert_eq!(nim.unwrap()["X-BILLING-INVOKE-ORIGIN"], "Pi");
        let gateway = default_attribution_headers(&model("x", "https://gateway.ai.cloudflare.com/v1/a/b"), true);
        assert_eq!(gateway.unwrap()["User-Agent"], "pi-coding-agent");
        assert!(default_attribution_headers(&model("openrouter", "https://openrouter.ai/api/v1"), false).is_none());
        assert!(default_attribution_headers(&model("openai", "https://api.openai.com/v1"), true).is_none());
        assert!(default_attribution_headers(&model("x", "https://notopenrouter.example/v1"), true).is_none());
    }

    #[test]
    fn merge_orders_sources_and_adds_opencode_sessions() {
        let model = model("openrouter", "https://openrouter.ai/api/v1");
        let provider = headers(&[("X-OpenRouter-Title", "custom"), ("X-Provider", "1")]);
        let request = headers(&[("X-Provider", "2")]);
        let merged =
            merge_provider_attribution_headers(&model, true, Some("s"), &[Some(&provider), None, Some(&request)]);
        let merged = merged.unwrap();
        assert_eq!((merged["X-OpenRouter-Title"].as_str(), merged["X-Provider"].as_str()), ("custom", "2"));
        assert!(!merged.contains_key("x-opencode-session"));
        assert!(merge_provider_attribution_headers(&model, false, None, &[]).is_none());
        let opencode = test_model("openai-completions", "custom", "https://opencode.ai/zen/v1");
        let merged = merge_provider_attribution_headers(&opencode, false, Some("abc"), &[]).unwrap();
        assert_eq!((merged["x-opencode-session"].as_str(), merged["x-opencode-client"].as_str()), ("abc", "pi"));
        assert!(session_headers(&opencode, None).is_none());
    }
}
