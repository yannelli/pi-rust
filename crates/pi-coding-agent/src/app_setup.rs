//! Startup choices for a coding session (upstream `main.ts` `buildSessionOptions` and
//! `sdk.ts` `createAgentSession`): model, thinking level, `--models` scope, tools, and prompts.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use pi_agent::{ConvertToLlmFn, SharedTool, default_convert_to_llm};
use pi_ai::auth::AuthResolver;
use pi_ai::models::{clamp_thinking_level, models_are_equal};
use pi_ai::{ContentBlock, Message, Model, ModelRegistry, ThinkingLevel, UserContent};

use crate::compaction::CompactionSettings;
use crate::model_resolver::{
    InitialModelOptions, ModelCatalog, ScopedModel, find_initial_model, resolve_cli_model, resolve_model_scope,
};
use crate::settings::tools::{DEFAULT_TOOL_NAMES, select_tools};
use crate::settings::{DEFAULT_THINKING_LEVEL, SettingsManager};
use crate::tools::{self, ALL_TOOLS, BashTool};

const DOCS_URL: &str = "https://github.com/yannelli/pi-rust";

/// Startup failed; `warnings` were collected before the errors and belong before them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupError {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.errors.join("\n"))
    }
}

impl std::error::Error for StartupError {}

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

pub(super) fn oauth_failed_message(provider: &str) -> String {
    format!(
        "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable. Run '/login {provider}' to re-authenticate."
    )
}

/// `--system-prompt` and `--append-system-prompt` values: the contents of an existing file
/// (relative to `cwd`), or the text itself.
pub fn resolve_prompt_input(input: &str, cwd: &Path) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let path = cwd.join(input);
    if !path.exists() {
        return Some(input.to_string());
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text)),
        Err(error) => {
            eprintln!("Warning: Could not read prompt file {input}: {error}");
            Some(input.to_string())
        }
    }
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
pub(super) struct CliChoice {
    pub model: Option<Model>,
    pub thinking: Option<ThinkingLevel>,
    pub scoped: Vec<ScopedModel>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

pub(super) struct CliInputs<'a> {
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub thinking: Option<ThinkingLevel>,
    pub models: Option<&'a [String]>,
    pub has_existing_session: bool,
}

pub(super) fn choose_cli_model(
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

/// The final model and thinking level (`createAgentSession`).
#[derive(Clone, Debug, Default)]
pub(super) struct SessionChoice {
    pub model: Option<Model>,
    pub thinking: ThinkingLevel,
    pub fallback_message: Option<String>,
}

pub(super) struct RestoredSelection<'a> {
    pub has_existing_session: bool,
    pub model: Option<&'a (String, String)>,
    /// The level of the latest `thinking_level_change` entry, if there is one.
    pub thinking: Option<ThinkingLevel>,
}

pub(super) fn choose_session_model(
    cli: &CliChoice,
    restored: &RestoredSelection<'_>,
    settings: &SettingsManager,
    catalog: &dyn ModelCatalog,
) -> SessionChoice {
    let mut model = cli.model.clone();
    let mut fallback_message = None;
    if model.is_none()
        && restored.has_existing_session
        && let Some((provider, id)) = restored.model
    {
        model = catalog.find_model(provider, id).filter(|model| catalog.has_configured_auth(&model.provider));
        if model.is_none() {
            fallback_message = Some(format!("Could not restore model {provider}/{id}"));
        }
    }
    if model.is_none() {
        let levels = settings.all_model_thinking_levels();
        let (provider, id) = (settings.default_provider(), settings.default_model());
        let options = InitialModelOptions {
            is_continuing: restored.has_existing_session,
            default_provider: provider.as_deref(),
            default_model_id: id.as_deref(),
            default_thinking_level: settings.default_thinking_level(),
            model_thinking_levels: Some(&levels),
            ..InitialModelOptions::default()
        };
        model = find_initial_model(&options, catalog).ok().and_then(|result| result.model);
        fallback_message = match (&model, fallback_message) {
            (None, _) => Some(format_no_models_available_message()),
            (Some(found), Some(message)) => Some(format!("{message}. Using {}/{}", found.provider, found.id)),
            (Some(_), None) => None,
        };
    }
    let mut thinking = cli.thinking;
    if thinking.is_none() && restored.has_existing_session {
        thinking = restored.thinking.or(settings.default_thinking_level()).or(Some(DEFAULT_THINKING_LEVEL));
    }
    if thinking.is_none()
        && let Some(model) = &model
    {
        thinking = settings.model_thinking_level(&model.provider, &model.id);
    }
    let thinking = thinking.or(settings.default_thinking_level()).unwrap_or(DEFAULT_THINKING_LEVEL);
    let thinking = model.as_ref().map_or(ThinkingLevel::Off, |model| clamp_thinking_level(model, thinking));
    SessionChoice { model, thinking, fallback_message }
}

/// Built-in tool names to enable: `defaultTools`, `--tools`, `--exclude-tools`, and
/// `--no-tools`/`--no-builtin-tools`. Names that are not built-in tools are dropped.
pub(super) fn choose_tools(
    settings: &SettingsManager,
    tools: Option<&[String]>,
    exclude: &[String],
    no_tools: bool,
) -> Vec<String> {
    let defaults =
        settings.default_tools().unwrap_or_else(|| DEFAULT_TOOL_NAMES.iter().map(|name| name.to_string()).collect());
    let mut selected = select_tools(&defaults, tools, exclude, no_tools, ALL_TOOLS);
    selected.retain(|name| ALL_TOOLS.contains(&name.as_str()));
    selected
}

const IMAGE_BLOCKED: &str = "Image reading is disabled.";

/// `images.blockImages`: replace image blocks sent to the model with a text placeholder.
pub(super) fn block_images_hook(settings: Arc<SettingsManager>) -> ConvertToLlmFn {
    Arc::new(move |messages: &[Message]| {
        let converted = default_convert_to_llm(messages);
        if !settings.block_images() {
            return converted;
        }
        converted
            .into_iter()
            .map(|message| match message {
                Message::User(mut user) => {
                    if let UserContent::Blocks(blocks) = &user.content {
                        user.content = UserContent::Blocks(without_images(blocks));
                    }
                    Message::User(user)
                }
                Message::ToolResult(mut result) => {
                    result.content = without_images(&result.content);
                    Message::ToolResult(result)
                }
                other => other,
            })
            .collect()
    })
}

fn without_images(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    let mut kept: Vec<ContentBlock> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let blocked = matches!(block, ContentBlock::Image { .. });
        let previous_blocked = matches!(kept.last(), Some(ContentBlock::Text { text, .. }) if text == IMAGE_BLOCKED);
        match (blocked, previous_blocked) {
            (true, true) => {}
            (true, false) => kept.push(ContentBlock::text(IMAGE_BLOCKED)),
            (false, _) => kept.push(block.clone()),
        }
    }
    kept
}

/// Built-in tools; `bash` uses the `shellPath` and `shellCommandPrefix` settings.
pub(super) fn build_tools(names: &[String], cwd: &Path, settings: &SettingsManager) -> anyhow::Result<Vec<SharedTool>> {
    names
        .iter()
        .map(|name| match name.as_str() {
            "bash" => Ok(Arc::new(
                BashTool::new(cwd.to_path_buf())
                    .with_shell_path(settings.shell_path())
                    .with_command_prefix(settings.shell_command_prefix()),
            ) as SharedTool),
            _ => tools::create_tool(name, cwd).ok_or_else(|| anyhow::anyhow!("Unknown tool name: {name}")),
        })
        .collect()
}

pub(super) fn compaction_settings(settings: &SettingsManager, model: &Model) -> CompactionSettings {
    let config = settings.compaction_settings(Some(model));
    CompactionSettings {
        enabled: config.enabled,
        reserve_tokens: config.reserve_tokens,
        keep_recent_tokens: config.keep_recent_tokens,
    }
}
