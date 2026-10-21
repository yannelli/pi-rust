//! Port of upstream `test/identity.test.ts` ("client identity"); request tests are in
//! `providers/openai_completions/tests_identity.rs`.

use serde_json::json;

use super::*;

fn acme() -> ClientAttribution {
    ClientAttribution { name: "acme".into(), url: "https://acme.example".into(), categories: None }
}

fn target<'a>(provider: &'a str, base_url: &'a str) -> ClientIdentityTarget<'a> {
    ClientIdentityTarget { provider, base_url }
}

fn pi() -> Option<ClientIdentity> {
    Some(ClientIdentity::attributed(pi_attribution()))
}

fn with(attribution: ClientAttribution) -> Option<ClientIdentity> {
    Some(ClientIdentity::attributed(attribution))
}

#[test]
fn uses_the_crate_version() {
    assert_eq!(PI_AI_VERSION, env!("CARGO_PKG_VERSION"));
}

#[test]
fn formats_user_agents() {
    let (platform, arch) = (node_platform(), node_arch());
    assert_eq!(format_user_agent(None), format!("pi/{PI_AI_VERSION} ({platform}; rust; {arch})"));
    let versioned = ClientIdentity { version: Some("9.9.9".into()), ..ClientIdentity::default() };
    assert_eq!(format_user_agent(Some(&versioned)), format!("pi/9.9.9 ({platform}; rust; {arch})"));
    let short = ClientIdentity { user_agent: Some(UserAgentMode::Short), ..versioned };
    assert_eq!(format_user_agent(Some(&short)), "pi");
    let custom = ClientIdentity { user_agent: Some(UserAgentMode::Custom("acme/1.0".into())), ..Default::default() };
    assert_eq!(format_user_agent(Some(&custom)), "acme/1.0");
}

#[test]
fn merges_identities_field_by_field() {
    assert_eq!(merge_client_identity([None, None]), None);
    let base = ClientIdentity { user_agent: Some(UserAgentMode::Short), ..pi().unwrap() };
    let version = ClientIdentity { version: Some("1.0.0".into()), ..ClientIdentity::default() };
    let expected = ClientIdentity { version: Some("1.0.0".into()), ..base.clone() };
    assert_eq!(merge_client_identity([Some(&base), Some(&version)]), Some(expected));
    let off = ClientIdentity { attribution: Some(None), ..ClientIdentity::default() };
    assert_eq!(merge_client_identity([pi().as_ref(), Some(&off)]), Some(off.clone()));
}

#[test]
fn identities_are_plain_json() {
    let identity = ClientIdentity {
        user_agent: Some(UserAgentMode::Custom("acme/1.0".into())),
        attribution: Some(Some(pi_attribution())),
        version: Some("2.1.0".into()),
    };
    let value = json!({
        "userAgent": {"custom": "acme/1.0"},
        "attribution": {"name": "pi", "url": "https://pi.dev", "categories": ["cli-agent"]},
        "version": "2.1.0",
    });
    assert_eq!(serde_json::to_value(&identity).unwrap(), value);
    assert_eq!(serde_json::from_value::<ClientIdentity>(value).unwrap(), identity);
    let off: ClientIdentity = serde_json::from_value(json!({"userAgent": "short", "attribution": null})).unwrap();
    assert_eq!((off.user_agent, off.attribution), (Some(UserAgentMode::Short), Some(None)));
    assert_eq!(
        serde_json::to_value(ClientIdentity { attribution: Some(None), ..Default::default() }).unwrap(),
        json!({"attribution": null})
    );
    assert_eq!(serde_json::from_value::<ClientIdentity>(json!({})).unwrap(), ClientIdentity::default());
}

#[test]
fn sends_no_attribution_unless_enabled() {
    let openrouter = target("openrouter", "https://openrouter.ai/api/v1");
    assert!(attribution_headers(openrouter, None).is_empty());
    let off = ClientIdentity { attribution: Some(None), ..ClientIdentity::default() };
    assert!(attribution_headers(openrouter, Some(&off)).is_empty());
    assert_eq!(identity_headers(openrouter, None), [("User-Agent", format_user_agent(None))]);
}

#[test]
fn attributes_openrouter_requests() {
    let openrouter = target("openrouter", "https://openrouter.ai/api/v1");
    let headers = |identity: Option<ClientIdentity>| attribution_headers(openrouter, identity.as_ref());
    assert_eq!(
        headers(pi()),
        [
            ("HTTP-Referer", "https://pi.dev".to_string()),
            ("X-OpenRouter-Title", "pi".into()),
            ("X-OpenRouter-Categories", "cli-agent".into())
        ]
    );
    let categories = Some(vec!["cli-agent".to_string(), "cloud-agent".to_string()]);
    assert_eq!(
        headers(with(ClientAttribution { categories, ..acme() })),
        [
            ("HTTP-Referer", "https://acme.example".to_string()),
            ("X-OpenRouter-Title", "acme".into()),
            ("X-OpenRouter-Categories", "cli-agent,cloud-agent".into())
        ]
    );
    let empty = ClientAttribution { categories: Some(Vec::new()), ..acme() };
    assert_eq!(
        headers(with(empty)),
        [("HTTP-Referer", "https://acme.example".to_string()), ("X-OpenRouter-Title", "acme".into())]
    );
    // Custom providers routed through OpenRouter, including legacy base URLs that are not valid URLs.
    let legacy = attribution_headers(target("custom", "not-a-url-openrouter.ai"), pi().as_ref());
    assert!(legacy.contains(&("X-OpenRouter-Title", "pi".into())));
}

#[test]
fn attributes_nvidia_with_a_capitalized_name_and_opencode_with_the_name() {
    let headers = |provider, base_url, identity: Option<ClientIdentity>| {
        attribution_headers(target(provider, base_url), identity.as_ref())
    };
    assert_eq!(headers("nvidia", "https://example.test/v1", pi()), [("X-BILLING-INVOKE-ORIGIN", "Pi".to_string())]);
    let nim = headers("custom-nim", "https://integrate.api.nvidia.com/v1", with(acme()));
    assert_eq!(nim, [("X-BILLING-INVOKE-ORIGIN", "Acme".to_string())]);
    assert_eq!(headers("opencode", "https://opencode.ai/zen/v1", pi()), [("x-opencode-client", "pi".to_string())]);
    let custom = headers("custom", "https://opencode.ai/zen/v1", with(acme()));
    assert_eq!(custom, [("x-opencode-client", "acme".to_string())]);
}

#[test]
fn does_not_attribute_nvidia_models_routed_through_gateways_to_nvidia() {
    let openrouter = attribution_headers(target("openrouter", "https://openrouter.ai/api/v1"), pi().as_ref());
    assert!(openrouter.iter().all(|(name, _)| *name != "X-BILLING-INVOKE-ORIGIN"));
    let vercel = target("vercel-ai-gateway", "https://ai-gateway.vercel.sh/v1");
    assert!(attribution_headers(vercel, pi().as_ref()).is_empty());
}

#[test]
fn adds_the_opencode_session_header_for_opencode_targets() {
    let custom = target("custom-opencode", "https://opencode.ai/zen/v1");
    assert_eq!(opencode_session_header(custom, Some("s-1")), Some(("x-opencode-session", "s-1".to_string())));
    assert_eq!(opencode_session_header(custom, None), None);
    assert_eq!(opencode_session_header(target("openai", "https://api.openai.com/v1"), Some("s-1")), None);
}
