//! `--provider`/`--model` resolution and the startup model choice.

use std::collections::BTreeMap;

use pi_ai::{Model, ThinkingLevel};

use super::{
    DEFAULT_THINKING_LEVEL, ModelCatalog, ModelReferenceRequest, ModelReferenceResult, ScopedModel, ThinkingSuffix,
    default_model_for, pick_default_model, resolve_model_reference,
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
    fn failed(error: String) -> Self {
        Self { error: Some(error), ..Self::default() }
    }
}

/// A model id the catalog does not list, built from the provider's default model.
fn fallback_model(provider_models: &[Model], id: &str) -> Model {
    let base = default_model_for(&provider_models[0].provider)
        .and_then(|default| provider_models.iter().find(|model| model.id == default))
        .unwrap_or(&provider_models[0]);
    Model { id: id.to_string(), name: id.to_string(), ..base.clone() }
}

/// Resolve `--provider`/`--model` through [`resolve_model_reference`], with custom ids for a
/// known provider. Searches all chat models, so `--api-key` works for first-time setup.
/// `inline_api_key` means `--api-key` was given; it goes to whichever provider is selected, so
/// stored credentials do not choose the provider.
pub fn resolve_cli_model(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    inline_api_key: bool,
    catalog: &dyn ModelCatalog,
) -> ResolveCliModelResult {
    let Some(cli_model) = cli_model else { return ResolveCliModelResult::default() };
    let models = catalog.all_chat_models();
    if models.is_empty() {
        return ResolveCliModelResult::failed(
            "No models available. Check your installation or add models to models.json.".into(),
        );
    }
    let provider = match cli_provider {
        Some(name) => match models.iter().find(|model| model.provider.eq_ignore_ascii_case(name)) {
            Some(model) => Some(model.provider.clone()),
            None => {
                return ResolveCliModelResult::failed(format!(
                    "Unknown provider \"{name}\". Use --list-models to see available providers/models."
                ));
            }
        },
        None => None,
    };
    let request = ModelReferenceRequest {
        text: cli_model,
        provider: provider.as_deref(),
        thinking_suffix: ThinkingSuffix::Strict,
        fuzzy: true,
    };
    let usable = |provider: &str| !inline_api_key && catalog.has_configured_auth(provider);
    match resolve_model_reference(&request, &models, &usable) {
        ModelReferenceResult::Resolved { model, thinking_level, warning } => {
            ResolveCliModelResult { model: Some(model), thinking_level, warning, error: None }
        }
        ModelReferenceResult::Ambiguous { candidates, usable_count, .. } => {
            let mut names: Vec<String> = candidates.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect();
            names.sort();
            let hint = if inline_api_key {
                "--api-key does not choose a provider."
            } else if usable_count == 0 {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            ResolveCliModelResult::failed(format!(
                "Model \"{cli_model}\" is ambiguous across providers: {}. {hint} Use --provider or provider/model.",
                names.join(", ")
            ))
        }
        ModelReferenceResult::NotFound { target: None } => ResolveCliModelResult::failed(format!(
            "Model \"{cli_model}\" not found. Use --list-models to see available models."
        )),
        ModelReferenceResult::NotFound { target: Some(target) } => {
            // With --thinking the suffix stays part of the custom id.
            let (id, level) = match cli_thinking {
                Some(_) => (target.model_text, None),
                None => (target.model_id, target.thinking_level),
            };
            let mut model = fallback_model(&target.provider_models, &id);
            if cli_thinking.or(level).is_some_and(|level| level != ThinkingLevel::Off) {
                model.reasoning = true;
            }
            ResolveCliModelResult {
                model: Some(model),
                thinking_level: level,
                warning: Some(format!(
                    "Model \"{id}\" not found for provider \"{}\". Using custom model id.",
                    target.provider
                )),
                error: None,
            }
        }
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
        let resolved = resolve_cli_model(options.cli_provider, Some(cli_model), None, false, catalog);
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
