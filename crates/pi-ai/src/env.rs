//! API keys from environment variables, using the same names as the TypeScript pi
//! (`ai/src/env-api-keys.ts`), plus the environment overlay applied to child processes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

use crate::auth::ProviderEnv;

pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ANTHROPIC_OAUTH_TOKEN_ENV: &str = "ANTHROPIC_OAUTH_TOKEN";
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const ANTHROPIC_FEDERATION_RULE_ID_ENV: &str = "ANTHROPIC_FEDERATION_RULE_ID";
pub const ANTHROPIC_ORGANIZATION_ID_ENV: &str = "ANTHROPIC_ORGANIZATION_ID";
pub const ANTHROPIC_SERVICE_ACCOUNT_ID_ENV: &str = "ANTHROPIC_SERVICE_ACCOUNT_ID";
pub const ANTHROPIC_IDENTITY_TOKEN_FILE_ENV: &str = "ANTHROPIC_IDENTITY_TOKEN_FILE";
pub const ANTHROPIC_WORKSPACE_ID_ENV: &str = "ANTHROPIC_WORKSPACE_ID";

/// Value returned for providers authenticated by ambient credentials (AWS, Google ADC).
pub const AMBIENT_CREDENTIAL: &str = "<authenticated>";

/// Environment variables that can hold an API key, by provider, in priority order.
/// `ANTHROPIC_AUTH_TOKEN` is listed for discovery; requests send it as a bearer token.
pub const API_KEY_ENV_VARS: &[(&str, &[&str])] = &[
    ("github-copilot", &["COPILOT_GITHUB_TOKEN"]),
    ("anthropic", &[ANTHROPIC_AUTH_TOKEN_ENV, ANTHROPIC_OAUTH_TOKEN_ENV, ANTHROPIC_API_KEY_ENV]),
    ("ant-ling", &["ANT_LING_API_KEY"]),
    ("qwen-token-plan", &["QWEN_TOKEN_PLAN_API_KEY"]),
    ("qwen-token-plan-individual", &["QWEN_TOKEN_PLAN_API_KEY"]),
    ("qwen-token-plan-cn", &["QWEN_TOKEN_PLAN_CN_API_KEY"]),
    ("openai", &["OPENAI_API_KEY"]),
    ("azure", &["AZURE_OPENAI_API_KEY"]),
    ("nvidia", &["NVIDIA_API_KEY"]),
    ("deepseek", &["DEEPSEEK_API_KEY"]),
    ("google", &["GEMINI_API_KEY"]),
    ("google-vertex", &["GOOGLE_CLOUD_API_KEY"]),
    ("groq", &["GROQ_API_KEY"]),
    ("cerebras", &["CEREBRAS_API_KEY"]),
    ("xai", &["XAI_API_KEY"]),
    ("typesafe", &["TYPESAFE_API_KEY"]),
    ("radius", &["RADIUS_API_KEY"]),
    ("openrouter", &["OPENROUTER_API_KEY"]),
    ("vercel-ai-gateway", &["AI_GATEWAY_API_KEY"]),
    ("zai", &["ZAI_API_KEY"]),
    ("zai-coding-cn", &["ZAI_CODING_CN_API_KEY"]),
    ("mistral", &["MISTRAL_API_KEY"]),
    ("minimax", &["MINIMAX_API_KEY"]),
    ("minimax-cn", &["MINIMAX_CN_API_KEY"]),
    ("moonshotai", &["MOONSHOT_API_KEY"]),
    ("moonshotai-cn", &["MOONSHOT_API_KEY"]),
    ("huggingface", &["HF_TOKEN"]),
    ("fireworks", &["FIREWORKS_API_KEY"]),
    ("together", &["TOGETHER_API_KEY"]),
    ("baseten", &["BASETEN_API_KEY"]),
    ("opencode", &["OPENCODE_API_KEY"]),
    ("opencode-go", &["OPENCODE_API_KEY"]),
    ("kimi-coding", &["KIMI_API_KEY"]),
    ("meta", &["META_API_KEY"]),
    ("cloudflare-workers-ai", &["CLOUDFLARE_API_KEY"]),
    ("cloudflare-ai-gateway", &["CLOUDFLARE_API_KEY"]),
    ("xiaomi", &["XIAOMI_API_KEY"]),
    ("xiaomi-token-plan-cn", &["XIAOMI_TOKEN_PLAN_CN_API_KEY"]),
    ("xiaomi-token-plan-ams", &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]),
    ("xiaomi-token-plan-sgp", &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]),
];

/// Variables that hold, locate, or scope ambient credentials: AWS for Bedrock, Google
/// application default credentials for Vertex, Azure OpenAI, Cloudflare, and Anthropic
/// workload identity federation.
pub const AMBIENT_CREDENTIAL_ENV_VARS: &[&str] = &[
    "AWS_PROFILE",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_CREDENTIAL_EXPIRATION",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_LOGIN_CACHE_DIRECTORY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_API_CERTIFICATE_CONFIG",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_QUOTA_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "GCLOUD_PROJECT",
    "CLOUDSDK_CONFIG",
    "AZURE_OPENAI_BASE_URL",
    "AZURE_OPENAI_RESOURCE_NAME",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_GATEWAY_ID",
    ANTHROPIC_FEDERATION_RULE_ID_ENV,
    ANTHROPIC_ORGANIZATION_ID_ENV,
    ANTHROPIC_SERVICE_ACCOUNT_ID_ENV,
    ANTHROPIC_IDENTITY_TOKEN_FILE_ENV,
    ANTHROPIC_WORKSPACE_ID_ENV,
];

/// Environment variables that can hold an API key for `provider`, in priority order.
pub fn api_key_env_vars(provider: &str) -> &'static [&'static str] {
    API_KEY_ENV_VARS.iter().find(|(id, _)| *id == provider).map_or(&[], |(_, names)| names)
}

/// Every variable the credential resolver and the providers read credentials from: the API
/// key variables of all providers and [`AMBIENT_CREDENTIAL_ENV_VARS`]. Tests strip these.
pub fn credential_env_vars() -> BTreeSet<&'static str> {
    let keys = API_KEY_ENV_VARS.iter().flat_map(|(_, names)| names.iter().copied());
    keys.chain(AMBIENT_CREDENTIAL_ENV_VARS.iter().copied()).collect()
}

/// A non-blank environment value. Provider-scoped `env` values win over the process environment.
pub fn provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    let scoped = env.and_then(|env| env.get(name)).filter(|value| !value.trim().is_empty()).cloned();
    scoped.or_else(|| std::env::var(name).ok().filter(|value| !value.trim().is_empty()))
}

/// Key variables for `provider` that are set, or `None` when none are.
pub fn find_env_keys(provider: &str, env: Option<&ProviderEnv>) -> Option<Vec<&'static str>> {
    let found: Vec<&'static str> =
        api_key_env_vars(provider).iter().copied().filter(|name| provider_env_value(name, env).is_some()).collect();
    (!found.is_empty()).then_some(found)
}

/// API key for `provider` from its environment variables. Skips `ANTHROPIC_AUTH_TOKEN`, which
/// requests send as `Authorization: Bearer`. Ambient Vertex and Bedrock credentials yield
/// [`AMBIENT_CREDENTIAL`].
pub fn get_env_api_key_with(provider: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(keys) = find_env_keys(provider, env) {
        let name = keys.into_iter().find(|name| provider != "anthropic" || *name != ANTHROPIC_AUTH_TOKEN_ENV);
        if let Some(value) = name.and_then(|name| provider_env_value(name, env)) {
            return Some(value);
        }
    }
    let set = |name: &str| provider_env_value(name, env).is_some();
    let ambient = match provider {
        "google-vertex" => {
            vertex_adc_credentials_exist(env)
                && (set("GOOGLE_CLOUD_PROJECT") || set("GCLOUD_PROJECT"))
                && set("GOOGLE_CLOUD_LOCATION")
        }
        "amazon-bedrock" => {
            set("AWS_PROFILE")
                || (set("AWS_ACCESS_KEY_ID") && set("AWS_SECRET_ACCESS_KEY"))
                || set("AWS_BEARER_TOKEN_BEDROCK")
                || set("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
                || set("AWS_CONTAINER_CREDENTIALS_FULL_URI")
                || set("AWS_WEB_IDENTITY_TOKEN_FILE")
        }
        _ => false,
    };
    ambient.then(|| AMBIENT_CREDENTIAL.to_string())
}

/// API key for `provider` from the process environment.
pub fn get_env_api_key(provider: &str) -> Option<String> {
    get_env_api_key_with(provider, None)
}

fn vertex_adc_credentials_exist(env: Option<&ProviderEnv>) -> bool {
    if let Some(path) = provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env) {
        return PathBuf::from(path).exists();
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(PathBuf::from)
        .is_some_and(|home| home.join(".config/gcloud/application_default_credentials.json").exists())
}

static CHILD_PROCESS_ENV: LazyLock<RwLock<BTreeMap<String, String>>> = LazyLock::new(Default::default);

/// Set a variable passed to every child process this program starts (bash commands,
/// `!command` config values). Used for process markers such as `AI_AGENT=pi`.
pub fn set_child_process_env(name: impl Into<String>, value: impl Into<String>) {
    CHILD_PROCESS_ENV.write().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(name.into(), value.into());
}

/// Variables set with [`set_child_process_env`].
pub fn child_process_env() -> BTreeMap<String, String> {
    CHILD_PROCESS_ENV.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// The whole environment as seen by this process: the real environment with the child overlay
/// on top, as upstream's mutated `process.env`.
pub fn process_env() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.extend(child_process_env());
    env
}

/// Value of `name` as seen by this process: the child overlay first, then the real environment.
pub fn process_env_value(name: &str) -> Option<String> {
    let overlay = CHILD_PROCESS_ENV.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(name).cloned();
    overlay.or_else(|| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_upstream_providers() {
        for (provider, name) in [
            ("ant-ling", "ANT_LING_API_KEY"),
            ("qwen-token-plan-individual", "QWEN_TOKEN_PLAN_API_KEY"),
            ("zai-coding-cn", "ZAI_CODING_CN_API_KEY"),
            ("minimax-cn", "MINIMAX_CN_API_KEY"),
            ("moonshotai-cn", "MOONSHOT_API_KEY"),
            ("opencode-go", "OPENCODE_API_KEY"),
            ("meta", "META_API_KEY"),
            ("xiaomi-token-plan-sgp", "XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
            ("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
        ] {
            assert_eq!(api_key_env_vars(provider), [name], "{provider}");
        }
        assert!(api_key_env_vars("unknown").is_empty());
    }

    #[test]
    fn scoped_env_wins_and_auth_token_is_skipped_for_keys() {
        let mut env = ProviderEnv::new();
        env.insert(ANTHROPIC_AUTH_TOKEN_ENV.into(), "bearer".into());
        assert_eq!(find_env_keys("anthropic", Some(&env)).unwrap(), [ANTHROPIC_AUTH_TOKEN_ENV]);
        assert_eq!(get_env_api_key_with("anthropic", Some(&env)).filter(|key| key == "bearer"), None);
        env.insert("PI_RUST_TEST_ONLY_KEY".into(), "scoped".into());
        assert_eq!(provider_env_value("PI_RUST_TEST_ONLY_KEY", Some(&env)).as_deref(), Some("scoped"));
        env.insert("AWS_PROFILE".into(), "dev".into());
        assert_eq!(get_env_api_key_with("amazon-bedrock", Some(&env)).as_deref(), Some(AMBIENT_CREDENTIAL));
    }

    /// Credential-like names in the provider code that only pick regions, endpoints, or switches.
    const NOT_CREDENTIALS: &[&str] = &[
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_ENDPOINT_URL",
        "AWS_ENDPOINT_URL_BEDROCK_RUNTIME",
        "AWS_ENDPOINT_URL_SSO",
        "AWS_ENDPOINT_URL_SSO_OIDC",
        "AWS_ENDPOINT_URL_STS",
        "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS",
        "AWS_USE_DUALSTACK_ENDPOINT",
        "AWS_USE_FIPS_ENDPOINT",
        "AWS_BEDROCK_FORCE_CACHE",
        "AWS_BEDROCK_SKIP_AUTH",
        "AWS_EC2_METADATA_DISABLED",
        "AWS_EC2_METADATA_SERVICE_ENDPOINT",
        "AZURE_OPENAI_API_VERSION",
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
        "GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES",
        "GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE",
        "GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL",
        "GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE",
        "GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE",
        "GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE",
    ];

    fn looks_like_credential(name: &str) -> bool {
        let prefixes = ["AWS_", "AZURE_", "ANTHROPIC_", "CLOUDFLARE_", "CLOUDSDK_", "GCLOUD_", "GOOGLE_"];
        let suffixes = ["_API_KEY", "_TOKEN", "_TOKEN_FILE", "_CREDENTIALS", "_SECRET"];
        name.starts_with(|first: char| first.is_ascii_uppercase())
            && !name.ends_with('_')
            && name.chars().all(|char| char.is_ascii_uppercase() || char.is_ascii_digit() || char == '_')
            && (prefixes.iter().any(|prefix| name.starts_with(prefix))
                || suffixes.iter().any(|suffix| name.ends_with(suffix)))
    }

    fn source_files(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let test =
                name == "tests" || name == "tests.rs" || name.starts_with("tests_") || name.ends_with("_tests.rs");
            if path.is_dir() && !test {
                source_files(&path, files);
            } else if name.ends_with(".rs") && !test {
                files.push(path);
            }
        }
    }

    #[test]
    fn credential_list_covers_every_credential_variable_the_providers_read() {
        let mut files = Vec::new();
        source_files(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let known = credential_env_vars();
        let mut missing = BTreeSet::new();
        for path in files {
            let source = std::fs::read_to_string(&path).unwrap();
            let parts: Vec<&str> = source.split('"').collect();
            for name in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
                if looks_like_credential(name) && !known.contains(name) && !NOT_CREDENTIALS.contains(name) {
                    missing.insert(format!("{name} ({})", path.file_name().unwrap().to_string_lossy()));
                }
            }
        }
        assert!(missing.is_empty(), "add to AMBIENT_CREDENTIAL_ENV_VARS or NOT_CREDENTIALS: {missing:?}");
        assert!(known.contains("OPENAI_API_KEY") && known.contains("HF_TOKEN") && known.contains("AWS_PROFILE"));
    }
}
