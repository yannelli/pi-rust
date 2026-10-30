use crate::auth::oauth::{BUILTIN_OAUTH_PROVIDER_IDS, builtin_oauth, builtin_oauth_providers, radius_oauth};

#[test]
fn builtin_flows_are_keyed_by_upstream_provider_ids() {
    let flows: Vec<(String, String, bool, Option<String>)> = builtin_oauth_providers()
        .into_iter()
        .map(|(id, oauth)| {
            (id, oauth.name().to_string(), oauth.is_subscription(), oauth.login_label().map(str::to_string))
        })
        .collect();
    let expected = [
        ("anthropic", "Anthropic (Claude Pro/Max)", true, None),
        ("github-copilot", "GitHub Copilot", true, None),
        ("kimi-coding", "Kimi Code (subscription)", true, Some("Sign in with Kimi Code")),
        ("meta", "Meta (Muse subscription)", true, Some("Sign in with Meta")),
        ("openai", "OpenAI (ChatGPT subscription)", true, Some("Sign in with ChatGPT")),
        ("openai-codex", "OpenAI (ChatGPT Plus/Pro)", true, None),
        ("openrouter", "OpenRouter OAuth", false, Some("Sign in with OpenRouter")),
        ("radius", "Radius", false, None),
        ("xai", "xAI (Grok/X subscription)", true, Some("Sign in with SuperGrok or X Premium")),
        ("cursor", "Cursor (subscription)", true, Some("Sign in with Cursor")),
        ("google-antigravity", "Google Antigravity", true, Some("Sign in with Google (Antigravity ACP)")),
    ];
    let expected: Vec<_> = expected
        .into_iter()
        .map(|(id, name, subscription, label)| {
            (id.to_string(), name.to_string(), subscription, label.map(str::to_string))
        })
        .collect();
    assert_eq!(flows, expected);
    assert_eq!(BUILTIN_OAUTH_PROVIDER_IDS.len(), expected.len());
    assert!(builtin_oauth("groq").is_none());
    assert_eq!(radius_oauth("Acme Gateway", "gw.example.com").name(), "Acme Gateway");
}
