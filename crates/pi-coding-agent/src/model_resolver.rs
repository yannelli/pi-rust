//! Model resolution, scoping, and startup selection (upstream `core/model-resolver.ts`):
//! exact `provider/id` references, fuzzy id and name matches that prefer aliases, globs,
//! `:thinking` suffixes, `--models` scopes, and the default model choice.

use std::cmp::Ordering;

use globset::GlobBuilder;
use pi_ai::models::models_are_equal;
use pi_ai::{Model, ModelRegistry, ThinkingLevel};

use crate::settings::DEFAULT_THINKING_LEVEL;

#[path = "model_resolver_cli.rs"]
mod cli;

pub use cli::{InitialModelOptions, InitialModelResult, ResolveCliModelResult, find_initial_model, resolve_cli_model};

/// Default chat model per provider, in upstream priority order.
pub const DEFAULT_MODEL_PER_PROVIDER: &[(&str, &str)] = &[
    ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", "claude-opus-4-8"),
    ("openai", "gpt-5.5"),
    ("azure", "gpt-5.4"),
    ("openai-codex", "gpt-6.1-sol"),
    ("radius", "balanced"),
    ("nvidia", "nvidia/nemotron-3.5-super-vl-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-3.1-pro-preview"),
    ("google-vertex", "gemini-3.1-pro-preview"),
    ("github-copilot", "gpt-5.4"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("vercel-ai-gateway", "zai/glm-5.1"),
    ("xai", "grok-4.7"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "gpt-oss-120b"),
    ("zai", "glm-5.3"),
    ("zai-coding-cn", "glm-5.3"),
    ("mistral", "devstral-medium-latest"),
    ("minimax", "MiniMax-M2.7"),
    ("minimax-cn", "MiniMax-M2.7"),
    ("moonshotai", "kimi-k2.6"),
    ("moonshotai-cn", "kimi-k2.6"),
    ("huggingface", "moonshotai/Kimi-K2.6"),
    ("fireworks", "accounts/fireworks/models/kimi-k3"),
    ("together", "moonshotai/Kimi-K3"),
    ("baseten", "zai-org/GLM-5.2"),
    ("opencode", "kimi-k2.6"),
    ("opencode-go", "kimi-k3"),
    ("kimi-coding", "kimi-for-coding"),
    ("meta", "muse-spark-1.3"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
    ("cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6"),
    ("qwen-token-plan", "qwen3.7-max"),
    ("qwen-token-plan-cn", "qwen3.7-max"),
    ("qwen-token-plan-individual", "qwen3.8-max"),
    ("xiaomi", "mimo-v2.6-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.6-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.6-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.6-pro"),
];

pub fn default_model_for(provider: &str) -> Option<&'static str> {
    DEFAULT_MODEL_PER_PROVIDER.iter().find(|(name, _)| *name == provider).map(|(_, id)| *id)
}

/// The models and auth state the resolver works with.
pub trait ModelCatalog {
    /// Every chat model, with or without auth.
    fn all_chat_models(&self) -> Vec<Model>;

    fn has_configured_auth(&self, provider: &str) -> bool;

    /// Chat models whose provider has configured auth.
    fn available_chat_models(&self) -> Vec<Model> {
        self.all_chat_models().into_iter().filter(|model| self.has_configured_auth(&model.provider)).collect()
    }

    fn find_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.all_chat_models().into_iter().find(|model| model.provider == provider && model.id == id)
    }
}

impl ModelCatalog for ModelRegistry {
    fn all_chat_models(&self) -> Vec<Model> {
        self.models().into_iter().cloned().collect()
    }

    fn has_configured_auth(&self, provider: &str) -> bool {
        self.has_api_key(provider)
    }

    fn find_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.find(provider, id).cloned()
    }
}

/// A model of a `--models` scope, with the level from a `:thinking` suffix.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedModel {
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
}

pub fn parse_thinking_level(value: &str) -> Option<ThinkingLevel> {
    value.parse().ok()
}

/// IDs ending in `-latest` or without a `-YYYYMMDD` date suffix.
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let bytes = id.as_bytes();
    let dated =
        bytes.len() >= 9 && bytes[bytes.len() - 9] == b'-' && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit);
    !dated
}

fn compare_ids(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

/// An exact `provider/id` or bare id match. Ambiguous bare ids match nothing.
pub fn find_exact_model_reference_match(reference: &str, models: &[Model]) -> Option<Model> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    let lower = reference.to_lowercase();
    let unique = |matches: Vec<&Model>| (matches.len() == 1).then(|| matches[0].clone());
    let canonical: Vec<&Model> =
        models.iter().filter(|model| format!("{}/{}", model.provider, model.id).to_lowercase() == lower).collect();
    if !canonical.is_empty() {
        return unique(canonical);
    }
    if let Some((provider, id)) = reference.split_once('/') {
        let (provider, id) = (provider.trim().to_lowercase(), id.trim().to_lowercase());
        if !provider.is_empty() && !id.is_empty() {
            let matches: Vec<&Model> = models
                .iter()
                .filter(|model| model.provider.to_lowercase() == provider && model.id.to_lowercase() == id)
                .collect();
            if !matches.is_empty() {
                return unique(matches);
            }
        }
    }
    unique(models.iter().filter(|model| model.id.to_lowercase() == lower).collect())
}

/// Exact reference first, then a case-insensitive id or name substring that prefers aliases
/// (the highest sorting), else the latest dated version.
fn try_match_model(pattern: &str, models: &[Model]) -> Option<Model> {
    if let Some(model) = find_exact_model_reference_match(pattern, models) {
        return Some(model);
    }
    let needle = pattern.to_lowercase();
    let matches: Vec<&Model> = models
        .iter()
        .filter(|model| model.id.to_lowercase().contains(&needle) || model.name.to_lowercase().contains(&needle))
        .collect();
    let (aliases, dated): (Vec<&Model>, Vec<&Model>) = matches.into_iter().partition(|model| is_alias(&model.id));
    let pool = if aliases.is_empty() { dated } else { aliases };
    pool.into_iter().max_by(|a, b| compare_ids(&a.id, &b.id)).cloned()
}

/// Result of [`parse_model_pattern`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedModelResult {
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub warning: Option<String>,
}

/// Match `pattern`, then retry without the last `:suffix`. A valid thinking level suffix is
/// returned; an invalid one warns, or with `allow_invalid_fallback` false, matches nothing.
pub fn parse_model_pattern(pattern: &str, models: &[Model], allow_invalid_fallback: bool) -> ParsedModelResult {
    if let Some(model) = try_match_model(pattern, models) {
        return ParsedModelResult { model: Some(model), ..ParsedModelResult::default() };
    }
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else { return ParsedModelResult::default() };
    if let Some(level) = parse_thinking_level(suffix) {
        let result = parse_model_pattern(prefix, models, allow_invalid_fallback);
        if result.model.is_some() {
            let thinking_level = if result.warning.is_some() { None } else { Some(level) };
            return ParsedModelResult { thinking_level, ..result };
        }
        return result;
    }
    if !allow_invalid_fallback {
        return ParsedModelResult::default();
    }
    let result = parse_model_pattern(prefix, models, allow_invalid_fallback);
    if result.model.is_some() {
        let warning = format!("Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead.");
        return ParsedModelResult { model: result.model, thinking_level: None, warning: Some(warning) };
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelScopeCode {
    NoMatch,
    InvalidThinkingLevel,
}

/// A warning about one `--models` or `enabledModels` pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelScopeDiagnostic {
    pub code: ModelScopeCode,
    pub message: String,
    pub pattern: String,
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(text))
}

fn push_unique(scoped: &mut Vec<ScopedModel>, model: Model, thinking_level: Option<ThinkingLevel>) {
    if !scoped.iter().any(|existing| models_are_equal(&existing.model, &model)) {
        scoped.push(ScopedModel { model, thinking_level });
    }
}

/// Resolve scope patterns (`provider/*`, `*sonnet*`, `sonnet:high`) against `models`.
pub fn resolve_model_scope(patterns: &[String], models: &[Model]) -> (Vec<ScopedModel>, Vec<ModelScopeDiagnostic>) {
    let mut scoped = Vec::new();
    let mut diagnostics = Vec::new();
    let no_match = |pattern: &str| ModelScopeDiagnostic {
        code: ModelScopeCode::NoMatch,
        message: format!("No models match pattern \"{pattern}\""),
        pattern: pattern.to_string(),
    };
    for pattern in patterns {
        if pattern.contains(['*', '?', '[']) {
            let (glob, level) = match pattern.rsplit_once(':') {
                Some((glob, suffix)) if parse_thinking_level(suffix).is_some() => (glob, parse_thinking_level(suffix)),
                _ => (pattern.as_str(), None),
            };
            if let Some(model) = find_exact_model_reference_match(glob, models) {
                push_unique(&mut scoped, model, level);
                continue;
            }
            let matching: Vec<&Model> = models
                .iter()
                .filter(|model| {
                    glob_matches(glob, &format!("{}/{}", model.provider, model.id)) || glob_matches(glob, &model.id)
                })
                .collect();
            if matching.is_empty() {
                diagnostics.push(no_match(pattern));
            }
            for model in matching {
                push_unique(&mut scoped, model.clone(), level);
            }
            continue;
        }
        let parsed = parse_model_pattern(pattern, models, true);
        if let Some(warning) = parsed.warning {
            diagnostics.push(ModelScopeDiagnostic {
                code: ModelScopeCode::InvalidThinkingLevel,
                message: warning,
                pattern: pattern.clone(),
            });
        }
        match parsed.model {
            Some(model) => push_unique(&mut scoped, model, parsed.thinking_level),
            None => diagnostics.push(no_match(pattern)),
        }
    }
    (scoped, diagnostics)
}

/// The preferred default among `available`: the first provider default that is present, else
/// the first available model.
pub fn pick_default_model(available: &[Model]) -> Option<Model> {
    DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find_map(|(provider, id)| available.iter().find(|model| model.provider == *provider && model.id == *id))
        .or_else(|| available.first())
        .cloned()
}

/// The default level when nothing else chooses one.
pub fn default_thinking_level() -> ThinkingLevel {
    DEFAULT_THINKING_LEVEL
}

#[cfg(test)]
#[path = "model_resolver_tests.rs"]
mod tests;
