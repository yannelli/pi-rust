//! Startup messages and the CLI model choice (upstream `main.ts` `buildSessionOptions`):
//! `--provider`, `--model`, `--thinking`, and the `--models` scope.

use std::collections::HashSet;

use pi_ai::auth::AuthResolver;
use pi_ai::models::models_are_equal;
use pi_ai::{Model, ModelRegistry, ThinkingLevel};

use crate::model_resolver::{ModelCatalog, ScopedModel, resolve_cli_model, resolve_model_scope};
use crate::settings::SettingsManager;

const DOCS_URL: &str = "https://github.com/yannelli/pi-rust";

pub fn provider_login_help() -> String {
    format!("Use /login to log into a provider via OAuth or API key. See:\n  {DOCS_URL}#providers\n  {DOCS_URL}#models")
}

pub fn format_no_models_available_message() -> String {
    format!("No models available. {}", provider_login_help())
}

pub fn format_no_api_key_found_message(provider: &str) -> String {
    let display = if provider == "unknown" { "the selected model" } else { provider };
    format!("No API key found for {display}.\n\n{}", provider_login_help())
}

pub fn oauth_failed_message(provider: &str) -> String {
    format!(
        "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable. Run '/login {provider}' to re-authenticate."
    )
}

/// Providers with usable auth, without refreshing tokens or running `models.json` commands.
pub async fn configured_providers(resolver: &AuthResolver, registry: &ModelRegistry) -> HashSet<String> {
    let mut configured = HashSet::new();
    for provider in registry.providers() {
        if matches!(resolver.check(&provider).await, Ok(Some(_))) {
            configured.insert(provider);
        }
    }
    configured
}

/// The CLI part of the model choice (`buildSessionOptions`).
#[derive(Clone, Debug, Default)]
pub(crate) struct CliChoice {
    pub model: Option<Model>,
    pub thinking: Option<ThinkingLevel>,
    pub scoped: Vec<ScopedModel>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

pub(crate) struct CliInputs<'a> {
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub thinking: Option<ThinkingLevel>,
    pub models: Option<&'a [String]>,
    pub has_existing_session: bool,
}

pub(crate) fn choose_cli_model(
    inputs: &CliInputs<'_>,
    settings: &SettingsManager,
    catalog: &dyn ModelCatalog,
) -> CliChoice {
    let mut choice = CliChoice::default();
    if let (Some(provider), None) = (inputs.provider, inputs.model) {
        choice
            .errors
            .push(format!("--provider requires --model (for example: --provider {provider} --model <pattern>)"));
    }
    let patterns = inputs.models.map(<[String]>::to_vec).or_else(|| settings.enabled_models()).unwrap_or_default();
    if !patterns.is_empty() {
        let (scoped, diagnostics) = resolve_model_scope(&patterns, &catalog.available_chat_models());
        choice.warnings.extend(diagnostics.into_iter().map(|diagnostic| diagnostic.message));
        choice.scoped = scoped;
    }
    if let Some(pattern) = inputs.model {
        let resolved = resolve_cli_model(inputs.provider, Some(pattern), inputs.thinking, catalog);
        choice.warnings.extend(resolved.warning);
        choice.errors.extend(resolved.error);
        if let Some(model) = resolved.model {
            choice.model = Some(model);
            if inputs.thinking.is_none() {
                choice.thinking = resolved.thinking_level;
            }
        }
    }
    if choice.model.is_none() && !choice.scoped.is_empty() && !inputs.has_existing_session {
        let saved = match (settings.default_provider(), settings.default_model()) {
            (Some(provider), Some(id)) => catalog.find_model(&provider, &id),
            _ => None,
        };
        let pick = saved
            .and_then(|saved| choice.scoped.iter().find(|scoped| models_are_equal(&scoped.model, &saved)))
            .unwrap_or(&choice.scoped[0])
            .clone();
        choice.model = Some(pick.model);
        if inputs.thinking.is_none() && pick.thinking_level.is_some() {
            choice.thinking = pick.thinking_level;
        }
    }
    if inputs.thinking.is_some() {
        choice.thinking = inputs.thinking;
    }
    choice
}
