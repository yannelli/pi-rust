//! API keys from environment variables, using the same names as the TypeScript pi.

/// Environment variables checked for `provider`, in priority order.
pub fn api_key_env_vars(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "azure" => &["AZURE_OPENAI_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "google" => &["GEMINI_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "moonshotai" => &["MOONSHOT_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "baseten" => &["BASETEN_API_KEY"],
        "opencode" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        _ => &[],
    }
}

/// The first non-empty API key variable set for `provider`.
pub fn get_env_api_key(provider: &str) -> Option<String> {
    api_key_env_vars(provider).iter().filter_map(|name| std::env::var(name).ok()).find(|value| !value.trim().is_empty())
}
