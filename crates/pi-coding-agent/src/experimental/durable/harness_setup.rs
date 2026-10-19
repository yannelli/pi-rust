//! Port of upstream `durable/harness-setup.ts`: pi's HTTP setup, Harness settings, the coding registry, one
//! execution environment per directory, and the start model of a new root conversation.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_ai::ThinkingLevel;
use pi_chord::Context;
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::settings::{
    CompactionOverrides, ConversationStreamOptions, HarnessSettings, RetryOverrides, SettingsSource,
};
use pi_durable::harness::types::{ModelRef, QueueMode};
use pi_durable::harness::{EnvTarget, Registry, create_registry};
use pi_durable::models::{Models, RegistryModels};
use pi_durable::{DurableError, DurableResult};

use super::prompt::create_pi_prompt;
use crate::agent_session::ModelRuntime;
use crate::model_resolver::{InitialModelOptions, find_initial_model, resolve_cli_model};
use crate::settings::SettingsManager;
use crate::settings::defaults::DEFAULT_HTTP_IDLE_TIMEOUT_MS;

/// pi's HTTP setup: proxy and idle timeouts for every HTTP client.
pub fn configure_harness_http(settings: &SettingsManager) {
    crate::agent_session::auth::configure_http_from_settings(settings);
}

fn queue_mode(mode: pi_agent::QueueMode) -> QueueMode {
    match mode {
        pi_agent::QueueMode::All => QueueMode::All,
        pi_agent::QueueMode::OneAtATime => QueueMode::OneAtATime,
    }
}

/// Harness settings read at every use from pi's settings as loaded at startup.
pub fn create_harness_settings(settings: Arc<SettingsManager>) -> SettingsSource {
    Arc::new(move || {
        let provider = settings.provider_retry_settings();
        // An invalid idle timeout falls back to the default, as pi's HTTP setup does.
        let idle = settings.http_idle_timeout_ms().unwrap_or(DEFAULT_HTTP_IDLE_TIMEOUT_MS);
        let compaction = settings.compaction_settings(None);
        let retry = settings.retry_settings();
        Ok(HarnessSettings {
            stream: Some(ConversationStreamOptions {
                timeout_ms: Some(provider.timeout_ms.unwrap_or(if idle == 0 { 2_147_483_647 } else { idle })),
                max_retry_delay_ms: Some(provider.max_retry_delay_ms),
                max_retries: provider.max_retries,
                ..ConversationStreamOptions::default()
            }),
            compaction: Some(CompactionOverrides {
                enabled: Some(compaction.enabled),
                reserve_tokens: Some(compaction.reserve_tokens),
                keep_recent_tokens: Some(compaction.keep_recent_tokens),
                background_tokens: None,
                model_overrides: None,
            }),
            retry: Some(RetryOverrides {
                enabled: Some(retry.enabled),
                max_retries: Some(retry.max_retries),
                base_delay_ms: Some(retry.base_delay_ms),
                max_agent_delay_ms: Some(retry.max_agent_delay_ms),
            }),
            steering_mode: Some(queue_mode(settings.steering_mode())),
            follow_up_mode: Some(queue_mode(settings.follow_up_mode())),
            ..HarnessSettings::default()
        })
    })
}

/// A registry with pi's coding tools and system prompt.
pub fn create_coding_registry(settings: Arc<SettingsManager>, agent_dir: &Path, cwd: &str) -> DurableResult<Registry> {
    let registry = create_registry();
    registry.install(pi_durable::tools::coding_tools())?;
    registry.install(create_pi_prompt(settings, agent_dir.to_path_buf(), cwd.to_string()))?;
    Ok(registry)
}

/// The Harness's model access over pi's model runtime: its catalog and its auth resolver.
pub fn runtime_models(runtime: &ModelRuntime) -> Arc<dyn Models> {
    Arc::new(RegistryModels::new(runtime.registry().clone(), runtime.resolver().clone()))
}

pub type EnvFuture = BoxFuture<'static, DurableResult<Option<SharedEnv>>>;

/// One execution environment per directory, shared by every conversation in it.
#[derive(Clone)]
pub struct ExecutionEnvs {
    default_cwd: String,
    envs: Arc<Mutex<HashMap<String, SharedEnv>>>,
}

impl ExecutionEnvs {
    pub fn new(default_cwd: impl Into<String>) -> Self {
        Self { default_cwd: default_cwd.into(), envs: Arc::default() }
    }

    pub fn env(&self, target: &EnvTarget) -> SharedEnv {
        let cwd = target.cwd.clone().unwrap_or_else(|| self.default_cwd.clone());
        let mut envs = self.envs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let local = || -> SharedEnv { Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(cwd.clone()))) };
        envs.entry(cwd.clone()).or_insert_with(local).clone()
    }

    /// The builder for `HarnessOptions::with_env`.
    pub fn builder(&self) -> impl Fn(EnvTarget, Context) -> EnvFuture + Send + Sync + 'static {
        let envs = self.clone();
        move |target, _| {
            let env = envs.env(&target);
            Box::pin(async move { Ok(Some(env)) })
        }
    }

    pub async fn cleanup(&self, context: &Context) {
        let envs: Vec<SharedEnv> = {
            let mut envs = self.envs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            envs.drain().map(|(_, env)| env).collect()
        };
        for env in envs {
            env.cleanup(context).await;
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InitialModel {
    pub model: Option<ModelRef>,
    pub thinking_level: Option<ThinkingLevel>,
    pub fallback_message: Option<String>,
}

/// The model a new root conversation starts with: an explicit `--provider`/`--model`, or pi's default resolution.
pub fn find_initial_agent_model(
    settings: &SettingsManager,
    runtime: &ModelRuntime,
    cli: Option<(Option<&str>, &str)>,
) -> DurableResult<InitialModel> {
    if let Some((provider, model)) = cli {
        let resolved = resolve_cli_model(provider, Some(model), None, false, runtime);
        let Some(found) = resolved.model.filter(|_| resolved.error.is_none()) else {
            let reason = resolved.error.unwrap_or_else(|| model.to_string());
            return Err(DurableError::Error(format!("Could not resolve model: {reason}")));
        };
        return Ok(InitialModel {
            model: Some(ModelRef { provider: found.provider, model_id: found.id }),
            thinking_level: resolved.thinking_level,
            fallback_message: None,
        });
    }
    let (provider, model_id) = (settings.default_provider(), settings.default_model());
    let options = InitialModelOptions {
        default_provider: provider.as_deref(),
        default_model_id: model_id.as_deref(),
        default_thinking_level: settings.default_thinking_level(),
        ..InitialModelOptions::default()
    };
    let found = find_initial_model(&options, runtime).map_err(DurableError::Error)?;
    Ok(match found.model {
        Some(model) => InitialModel {
            model: Some(ModelRef { provider: model.provider, model_id: model.id }),
            thinking_level: Some(found.thinking_level),
            fallback_message: None,
        },
        None => InitialModel::default(),
    })
}
