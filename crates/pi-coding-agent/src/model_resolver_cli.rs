//! `--provider`/`--model` resolution and the startup model choice.

use std::collections::BTreeMap;

use pi_ai::models::models_are_equal;
use pi_ai::{Model, ThinkingLevel};

use super::{
    DEFAULT_THINKING_LEVEL, ModelCatalog, ScopedModel, default_model_for, parse_model_pattern, parse_thinking_level,
    pick_default_model,
};

/// Result of [`resolve_cli_model`]. `error` is set exactly when `model` is `None`
/// (and `--model` was given).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolveCliModelResult {
    pub model: Option<Model>,
    /// Level parsed from a `<pattern>:<level>` suffix.
    pub thinking_level: Option<ThinkingLevel>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

impl ResolveCliModelResult {
    fn found(model: Model) -> Self {
        Self { model: Some(model), ..Self::default() }
    }

    fn failed(error: String) -> Self {
        Self { error: Some(error), ..Self::default() }
    }
}

/// A model id the catalog does not list, built from the provider's default model.
fn fallback_model(provider: &str, id: &str, models: &[Model]) -> Option<Model> {
    let own: Vec<&Model> = models.iter().filter(|model| model.provider == provider).collect();
    let base = default_model_for(provider)
        .and_then(|default| own.iter().find(|model| model.id == default))
        .or_else(|| own.first())?;
    Some(Model { id: id.to_string(), name: id.to_string(), ..(*base).clone() })
}

/// Resolve `--provider`/`--model`: `provider/pattern`, exact ids (ambiguous ones prefer the
/// one authenticated provider), fuzzy matches, `:thinking` suffixes, and custom ids for a
/// known provider. Searches all chat models, so `--api-key` works for first-time setup.
pub fn resolve_cli_model(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    catalog: &dyn ModelCatalog,
) -> ResolveCliModelResult {
    let Some(cli_model) = cli_model else { return ResolveCliModelResult::default() };
    let models = catalog.all_chat_models();
    if models.is_empty() {
        return ResolveCliModelResult::failed(
            "No models available. Check your installation or add models to models.json.".into(),
        );
    }
    let canonical =
        |name: &str| models.iter().find(|m| m.provider.eq_ignore_ascii_case(name)).map(|m| m.provider.clone());
    let mut provider = match cli_provider {
        Some(name) => match canonical(name) {
            Some(provider) => Some(provider),
            None => {
                return ResolveCliModelResult::failed(format!(
                    "Unknown provider \"{name}\". Use --list-models to see available providers/models."
                ));
            }
        },
        None => None,
    };
    let mut pattern = cli_model.to_string();
    let mut inferred = false;
    if provider.is_none()
        && let Some((prefix, rest)) = cli_model.split_once('/')
        && let Some(found) = canonical(prefix)
    {
        provider = Some(found);
        pattern = rest.to_string();
        inferred = true;
    }
    let lower = cli_model.to_lowercase();
    let is_exact = |model: &Model| {
        model.id.to_lowercase() == lower || format!("{}/{}", model.provider, model.id).to_lowercase() == lower
    };
    if provider.is_none() {
        let exact: Vec<&Model> = models.iter().filter(|model| is_exact(model)).collect();
        if exact.len() == 1 {
            return ResolveCliModelResult::found(exact[0].clone());
        }
        if exact.len() > 1 {
            let authenticated: Vec<&&Model> =
                exact.iter().filter(|m| catalog.has_configured_auth(&m.provider)).collect();
            if authenticated.len() == 1 {
                return ResolveCliModelResult::found((*authenticated[0]).clone());
            }
            let mut names: Vec<String> = exact.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect();
            names.sort();
            let hint = if authenticated.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            return ResolveCliModelResult::failed(format!(
                "Model \"{cli_model}\" is ambiguous across providers: {}. {hint} Use --provider or provider/model.",
                names.join(", ")
            ));
        }
    }
    if let (Some(_), Some(provider)) = (cli_provider, &provider) {
        let prefix = format!("{}/", provider.to_lowercase());
        if cli_model.to_lowercase().starts_with(&prefix) {
            pattern = cli_model[prefix.len()..].to_string();
        }
    }
    let candidates: Vec<Model> = match &provider {
        Some(provider) => models.iter().filter(|model| &model.provider == provider).cloned().collect(),
        None => models.clone(),
    };
    let parsed = parse_model_pattern(&pattern, &candidates, false);
    if let Some(model) = parsed.model {
        if inferred && !catalog.has_configured_auth(&model.provider) {
            let raw: Vec<&Model> = models
                .iter()
                .filter(|m| m.id.to_lowercase() == lower && !models_are_equal(m, &model))
                .filter(|m| catalog.has_configured_auth(&m.provider))
                .collect();
            if raw.len() == 1 {
                return ResolveCliModelResult::found(raw[0].clone());
            }
        }
        return ResolveCliModelResult {
            model: Some(model),
            thinking_level: parsed.thinking_level,
            warning: parsed.warning,
            error: None,
        };
    }
    if inferred {
        if let Some(model) = models.iter().find(|model| is_exact(model)) {
            return ResolveCliModelResult::found(model.clone());
        }
        let fallback = parse_model_pattern(cli_model, &models, false);
        if fallback.model.is_some() {
            return fallback_result(fallback);
        }
    }
    if let Some(provider) = &provider {
        let (mut id, mut level) = (pattern.clone(), None);
        if cli_thinking.is_none()
            && let Some((prefix, suffix)) = pattern.rsplit_once(':')
            && let Some(parsed_level) = parse_thinking_level(suffix)
        {
            id = prefix.to_string();
            level = Some(parsed_level);
        }
        if let Some(mut model) = fallback_model(provider, &id, &models) {
            if cli_thinking.or(level).is_some_and(|level| level != ThinkingLevel::Off) {
                model.reasoning = true;
            }
            let note = format!("Model \"{id}\" not found for provider \"{provider}\". Using custom model id.");
            let warning = match parsed.warning {
                Some(earlier) => format!("{earlier} {note}"),
                None => note,
            };
            return ResolveCliModelResult {
                model: Some(model),
                thinking_level: level,
                warning: Some(warning),
                error: None,
            };
        }
    }
    let display = provider.map_or_else(|| cli_model.to_string(), |provider| format!("{provider}/{pattern}"));
    ResolveCliModelResult {
        warning: parsed.warning,
        error: Some(format!("Model \"{display}\" not found. Use --list-models to see available models.")),
        ..ResolveCliModelResult::default()
    }
}

fn fallback_result(parsed: super::ParsedModelResult) -> ResolveCliModelResult {
    ResolveCliModelResult {
        model: parsed.model,
        thinking_level: parsed.thinking_level,
        warning: parsed.warning,
        error: None,
    }
}

/// Inputs to [`find_initial_model`].
#[derive(Clone, Debug, Default)]
pub struct InitialModelOptions<'a> {
    pub cli_provider: Option<&'a str>,
    pub cli_model: Option<&'a str>,
    pub scoped_models: &'a [ScopedModel],
    pub is_continuing: bool,
    pub default_provider: Option<&'a str>,
    pub default_model_id: Option<&'a str>,
    pub default_thinking_level: Option<ThinkingLevel>,
    pub model_thinking_levels: Option<&'a BTreeMap<String, ThinkingLevel>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InitialModelResult {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
}

/// The startup model: CLI flags, then the first scoped model (unless continuing), then the
/// saved default when its provider has auth, then provider defaults among models with auth.
pub fn find_initial_model(
    options: &InitialModelOptions<'_>,
    catalog: &dyn ModelCatalog,
) -> Result<InitialModelResult, String> {
    if let (Some(_), Some(cli_model)) = (options.cli_provider, options.cli_model) {
        let resolved = resolve_cli_model(options.cli_provider, Some(cli_model), None, catalog);
        if let Some(error) = resolved.error {
            return Err(error);
        }
        if resolved.model.is_some() {
            return Ok(InitialModelResult { model: resolved.model, thinking_level: DEFAULT_THINKING_LEVEL });
        }
    }
    let per_model = |model: &Model| {
        options
            .model_thinking_levels
            .and_then(|levels| levels.get(&format!("{}/{}", model.provider, model.id)).copied())
    };
    if let Some(scoped) = options.scoped_models.first().filter(|_| !options.is_continuing) {
        let level = scoped.thinking_level.or_else(|| per_model(&scoped.model)).or(options.default_thinking_level);
        return Ok(InitialModelResult {
            model: Some(scoped.model.clone()),
            thinking_level: level.unwrap_or(DEFAULT_THINKING_LEVEL),
        });
    }
    if let (Some(provider), Some(id)) = (options.default_provider, options.default_model_id)
        && let Some(model) =
            catalog.find_model(provider, id).filter(|model| catalog.has_configured_auth(&model.provider))
    {
        let level = per_model(&model).or(options.default_thinking_level).unwrap_or(DEFAULT_THINKING_LEVEL);
        return Ok(InitialModelResult { model: Some(model), thinking_level: level });
    }
    let model = pick_default_model(&catalog.available_chat_models());
    Ok(InitialModelResult { model, thinking_level: DEFAULT_THINKING_LEVEL })
}
