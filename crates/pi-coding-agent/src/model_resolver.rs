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

/// How [`resolve_model_reference`] treats a trailing `:<suffix>`. It is only considered when the
/// full text matches no model, since ids such as `qwen/qwen3-coder:exacto` contain colons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingSuffix {
    /// The suffix stays part of the id.
    None,
    /// A valid thinking level is parsed; any other suffix stays part of the id.
    Strict,
    /// Like `Strict`, but an invalid suffix is dropped with a warning.
    Lenient,
}

/// A model reference: a bare id or `provider/id`, optionally followed by `:<thinking>`.
#[derive(Clone, Copy, Debug)]
pub struct ModelReferenceRequest<'a> {
    pub text: &'a str,
    /// Only consider models from this provider.
    pub provider: Option<&'a str>,
    pub thinking_suffix: ThinkingSuffix,
    /// Match id and name substrings when no exact match exists.
    pub fuzzy: bool,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum ModelReferenceResult {
    Resolved {
        model: Model,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    Ambiguous {
        candidates: Vec<Model>,
        usable_count: usize,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    /// `target` is set when a provider is explicit or named by a `provider/` prefix.
    NotFound {
        target: Option<ModelReferenceTarget>,
    },
}

/// Where a custom model id goes when no model matches.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelReferenceTarget {
    pub provider: String,
    /// The provider's models, never empty.
    pub provider_models: Vec<Model>,
    /// Text after the provider prefix as typed, including any `:<suffix>`.
    pub model_text: String,
    /// `model_text` without a valid thinking suffix (unless the request uses [`ThinkingSuffix::None`]).
    pub model_id: String,
    pub thinking_level: Option<ThinkingLevel>,
}

impl ModelReferenceResult {
    const NOT_FOUND: Self = Self::NotFound { target: None };

    fn resolved(model: &Model) -> Self {
        Self::Resolved { model: model.clone(), thinking_level: None, warning: None }
    }

    fn annotations(&mut self) -> Option<(&mut Option<ThinkingLevel>, &mut Option<String>)> {
        match self {
            Self::Resolved { thinking_level, warning, .. } | Self::Ambiguous { thinking_level, warning, .. } => {
                Some((thinking_level, warning))
            }
            Self::NotFound { .. } => None,
        }
    }
}

fn split_thinking_suffix(text: &str) -> Option<(&str, ThinkingLevel)> {
    let (base, suffix) = text.rsplit_once(':')?;
    Some((base, parse_thinking_level(suffix)?))
}

fn split_provider_prefix<'a>(text: &'a str, models: &[Model]) -> Option<(String, &'a str)> {
    let (prefix, model_text) = text.split_once('/')?;
    let (prefix, model_text) = (prefix.trim().to_lowercase(), model_text.trim());
    if prefix.is_empty() || model_text.is_empty() {
        return None;
    }
    let provider = models.iter().find(|model| model.provider.to_lowercase() == prefix)?;
    Some((provider.provider.clone(), model_text))
}

/// Candidates are collected before a provider is chosen, so catalog order never decides the
/// provider and an ambiguous match is reported instead of falling through to a weaker one.
/// `usable` only breaks ties between providers, so a unique match resolves even without
/// credentials (`--api-key` first-time setup).
pub fn resolve_model_reference(
    request: &ModelReferenceRequest<'_>,
    models: &[Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    let text = request.text.trim();
    if text.is_empty() {
        return ModelReferenceResult::NOT_FOUND;
    }
    let constraint = request.provider.map(str::to_lowercase);
    let constrained: Vec<Model>;
    let pool = match &constraint {
        Some(provider) => {
            constrained = models.iter().filter(|model| model.provider.to_lowercase() == *provider).cloned().collect();
            &constrained[..]
        }
        None => models,
    };
    let result = resolve_text(text, request, pool, usable);
    if !matches!(result, ModelReferenceResult::NotFound { .. }) {
        return result;
    }
    let split = split_provider_prefix(text, pool);
    let anchor = match (&constraint, &split) {
        (Some(_), _) => pool.first(),
        (None, Some((provider, _))) => pool.iter().find(|model| model.provider == *provider),
        (None, None) => None,
    };
    let Some(anchor) = anchor else { return result };
    let model_text = split.as_ref().map_or(text, |(_, model_text)| model_text);
    let suffix = match request.thinking_suffix {
        ThinkingSuffix::None => None,
        _ => split_thinking_suffix(model_text),
    };
    ModelReferenceResult::NotFound {
        target: Some(ModelReferenceTarget {
            provider: anchor.provider.clone(),
            provider_models: pool.iter().filter(|model| model.provider == anchor.provider).cloned().collect(),
            model_text: model_text.to_string(),
            model_id: suffix.map_or(model_text, |(base, _)| base).to_string(),
            thinking_level: suffix.map(|(_, level)| level),
        }),
    }
}

/// An exact `provider/id` or bare id match without thinking suffixes or fuzzy matching.
pub fn find_exact_model_reference_match(reference: &str, models: &[Model]) -> Option<Model> {
    let request =
        ModelReferenceRequest { text: reference, provider: None, thinking_suffix: ThinkingSuffix::None, fuzzy: false };
    match resolve_model_reference(&request, models, &|_| true) {
        ModelReferenceResult::Resolved { model, .. } => Some(model),
        _ => None,
    }
}

fn resolve_text(
    text: &str,
    request: &ModelReferenceRequest<'_>,
    pool: &[Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    if let Some(reference) = select_reference(text, pool, request.fuzzy, usable) {
        return reference;
    }
    if request.fuzzy
        && let Some(fuzzy) = select_fuzzy(fuzzy_matches(text, pool), usable)
    {
        return fuzzy;
    }
    if request.thinking_suffix == ThinkingSuffix::None {
        return ModelReferenceResult::NOT_FOUND;
    }
    if let Some((base, level)) = split_thinking_suffix(text) {
        let mut inner = resolve_text(base, request, pool, usable);
        if let Some((thinking_level, warning)) = inner.annotations() {
            *thinking_level = if warning.is_some() { None } else { Some(level) };
        }
        return inner;
    }
    // Keep an invalid suffix as part of the id and fail, rather than resolve a different model.
    if request.thinking_suffix == ThinkingSuffix::Strict {
        return ModelReferenceResult::NOT_FOUND;
    }
    let Some((base, suffix)) = text.rsplit_once(':') else { return ModelReferenceResult::NOT_FOUND };
    let mut inner = resolve_text(base, request, pool, usable);
    if let Some((thinking_level, warning)) = inner.annotations() {
        *thinking_level = None;
        *warning = Some(format!("Invalid thinking level \"{suffix}\" in pattern \"{text}\". Using default instead."));
    }
    inner
}

fn distinct_providers(models: &[&Model]) -> usize {
    models.iter().map(|model| model.provider.as_str()).collect::<std::collections::HashSet<_>>().len()
}

/// `provider/rest` is only a hint: the whole text may also be a model id on another provider, such
/// as gateway id `zai/glm-5`. The named provider wins when usable; otherwise a sole usable raw match
/// wins. An unusable named match beats unusable raw matches, so `--api-key` can set it up.
fn select_reference(
    text: &str,
    pool: &[Model],
    fuzzy: bool,
    usable: &dyn Fn(&str) -> bool,
) -> Option<ModelReferenceResult> {
    let lower = text.to_lowercase();
    let raw: Vec<&Model> = pool.iter().filter(|model| model.id.to_lowercase() == lower).collect();
    let named = split_provider_prefix(text, pool).and_then(|(provider, model_text)| {
        let provider_models: Vec<&Model> = pool.iter().filter(|model| model.provider == provider).collect();
        let lower_model_text = model_text.to_lowercase();
        // The provider's own literal id (OpenRouter's `openrouter/free`) beats its fuzzy matches.
        provider_models
            .iter()
            .find(|model| model.id.to_lowercase() == lower_model_text)
            .or_else(|| raw.iter().find(|model| model.provider == provider))
            .copied()
            .or_else(|| fuzzy.then(|| best_fuzzy_match(&fuzzy_matches(model_text, provider_models))).flatten())
    });
    let others: Vec<&Model> =
        raw.into_iter().filter(|model| named.is_none_or(|named| !std::ptr::eq(*model, named))).collect();
    let candidates: Vec<&Model> = named.into_iter().chain(others.iter().copied()).collect();
    if candidates.is_empty() {
        return None;
    }
    if distinct_providers(&candidates) == 1 {
        return Some(ModelReferenceResult::resolved(candidates[0]));
    }
    if let Some(named) = named.filter(|named| usable(&named.provider)) {
        return Some(ModelReferenceResult::resolved(named));
    }
    let usable_others: Vec<&Model> = others.iter().copied().filter(|model| usable(&model.provider)).collect();
    match (usable_others.as_slice(), named) {
        ([only], _) => Some(ModelReferenceResult::resolved(only)),
        ([], Some(named)) => Some(ModelReferenceResult::resolved(named)),
        _ => Some(ModelReferenceResult::Ambiguous {
            candidates: candidates.into_iter().cloned().collect(),
            usable_count: usable_others.len(),
            thinking_level: None,
            warning: None,
        }),
    }
}

fn fuzzy_matches<'a>(text: &str, models: impl IntoIterator<Item = &'a Model>) -> Vec<&'a Model> {
    let needle = text.to_lowercase();
    models
        .into_iter()
        .filter(|model| model.id.to_lowercase().contains(&needle) || model.name.to_lowercase().contains(&needle))
        .collect()
}

fn select_fuzzy(matches: Vec<&Model>, usable: &dyn Fn(&str) -> bool) -> Option<ModelReferenceResult> {
    if matches.is_empty() {
        return None;
    }
    let usable_matches: Vec<&Model> = if distinct_providers(&matches) > 1 {
        matches.iter().copied().filter(|model| usable(&model.provider)).collect()
    } else {
        Vec::new()
    };
    let pool = if usable_matches.is_empty() { &matches } else { &usable_matches };
    let best = best_fuzzy_match(pool)?;
    // The same id on several providers would otherwise be decided by catalog order.
    let tied: Vec<&Model> = pool.iter().copied().filter(|model| model.id == best.id).collect();
    if distinct_providers(&tied) == 1 {
        return Some(ModelReferenceResult::resolved(best));
    }
    Some(ModelReferenceResult::Ambiguous {
        usable_count: if usable_matches.is_empty() { 0 } else { tied.len() },
        candidates: tied.into_iter().cloned().collect(),
        thinking_level: None,
        warning: None,
    })
}

/// Prefer aliases (`claude-sonnet-4-5`) over dated versions (`claude-sonnet-4-5-20250929`), then
/// the highest sorting id.
fn best_fuzzy_match<'a>(matches: &[&'a Model]) -> Option<&'a Model> {
    let aliases: Vec<&Model> = matches.iter().copied().filter(|model| is_alias(&model.id)).collect();
    let pool = if aliases.is_empty() { matches } else { &aliases };
    pool.iter().copied().min_by(|a, b| compare_ids(&b.id, &a.id))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelScopeCode {
    NoMatch,
    InvalidThinkingLevel,
    Ambiguous,
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

/// Resolve scope patterns (`provider/*`, `*sonnet*`, `sonnet:high`) against `models`. Every model
/// in `models` is usable, so a pattern matching the same id on several providers adds all of them,
/// with a warning so the user can pick one with `provider/id`.
pub fn resolve_model_scope(patterns: &[String], models: &[Model]) -> (Vec<ScopedModel>, Vec<ModelScopeDiagnostic>) {
    let mut scoped = Vec::new();
    let mut diagnostics = Vec::new();
    let no_match = |pattern: &str| ModelScopeDiagnostic {
        code: ModelScopeCode::NoMatch,
        message: format!("No models match pattern \"{pattern}\""),
        pattern: pattern.to_string(),
    };
    let ambiguous = |pattern: &str, candidates: &[Model]| {
        let mut providers: Vec<&str> = candidates.iter().map(|model| model.provider.as_str()).collect();
        providers.sort_unstable();
        providers.dedup();
        ModelScopeDiagnostic {
            code: ModelScopeCode::Ambiguous,
            message: format!(
                "Pattern \"{pattern}\" matches several providers: {}. Use provider/id to choose one.",
                providers.join(", ")
            ),
            pattern: pattern.to_string(),
        }
    };
    for pattern in patterns {
        if pattern.contains(['*', '?', '[']) {
            let (glob, level) =
                split_thinking_suffix(pattern).map_or((pattern.as_str(), None), |(base, level)| (base, Some(level)));
            // Ids such as `bracketed-model[1m]` are not globs.
            let exact = ModelReferenceRequest {
                text: glob,
                provider: None,
                thinking_suffix: ThinkingSuffix::None,
                fuzzy: false,
            };
            let matches = match resolve_model_reference(&exact, models, &|_| true) {
                ModelReferenceResult::Resolved { model, .. } => vec![model],
                ModelReferenceResult::Ambiguous { candidates, .. } => {
                    diagnostics.push(ambiguous(pattern, &candidates));
                    candidates
                }
                ModelReferenceResult::NotFound { .. } => models
                    .iter()
                    .filter(|model| {
                        glob_matches(glob, &format!("{}/{}", model.provider, model.id)) || glob_matches(glob, &model.id)
                    })
                    .cloned()
                    .collect(),
            };
            if matches.is_empty() {
                diagnostics.push(no_match(pattern));
            }
            for model in matches {
                push_unique(&mut scoped, model, level);
            }
            continue;
        }
        let request = ModelReferenceRequest {
            text: pattern,
            provider: None,
            thinking_suffix: ThinkingSuffix::Lenient,
            fuzzy: true,
        };
        let (matches, thinking_level, warning, is_ambiguous) =
            match resolve_model_reference(&request, models, &|_| true) {
                ModelReferenceResult::NotFound { .. } => {
                    diagnostics.push(no_match(pattern));
                    continue;
                }
                ModelReferenceResult::Resolved { model, thinking_level, warning } => {
                    (vec![model], thinking_level, warning, false)
                }
                ModelReferenceResult::Ambiguous { candidates, thinking_level, warning, .. } => {
                    (candidates, thinking_level, warning, true)
                }
            };
        if let Some(warning) = warning {
            diagnostics.push(ModelScopeDiagnostic {
                code: ModelScopeCode::InvalidThinkingLevel,
                message: warning,
                pattern: pattern.clone(),
            });
        }
        if is_ambiguous {
            diagnostics.push(ambiguous(pattern, &matches));
        }
        for model in matches {
            push_unique(&mut scoped, model, thinking_level);
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
