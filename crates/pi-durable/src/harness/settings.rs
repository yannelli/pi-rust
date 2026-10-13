//! Harness settings (upstream `HarnessSettings` and `Settings`) and their built-in defaults.

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_ai::{CacheRetention, Transport};
use pi_chord::JsonMap;
use serde::{Deserialize, Serialize};

use super::define::Extension;
use super::types::{QueueMode, ToolExecutionMode};
use crate::errors::DurableResult;
use crate::models::DeferredOption;

/// Curated pi-ai request options; absent fields use pi-ai defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStreamOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredOption>,
}

/// Durable retries of generation requests; the JSON shape of pi-ai `RetryPolicy`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRetryPolicy {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<u64>,
}

/// Automatic compaction thresholds (spec section 8.7); manual compaction ignores `enabled`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPolicy {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub background_tokens: u64,
}

/// How often running progress is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPolicy {
    pub partial_interval_ms: u64,
    pub output_interval_ms: u64,
}

pub const DEFAULT_RETRY_POLICY: ConversationRetryPolicy =
    ConversationRetryPolicy { enabled: true, max_retries: 3, base_delay_ms: 2000, max_agent_delay_ms: Some(60000) };

pub const DEFAULT_COMPACTION_POLICY: CompactionPolicy =
    CompactionPolicy { enabled: true, reserve_tokens: 16384, keep_recent_tokens: 20000, background_tokens: 32768 };

pub const DEFAULT_PROGRESS_POLICY: ProgressPolicy = ProgressPolicy { partial_interval_ms: 100, output_interval_ms: 100 };

/// Default `context_retention_ms`: ten minutes.
pub const DEFAULT_CONTEXT_RETENTION_MS: f64 = 600_000.0;

/// Partial retry policy of [`HarnessSettings`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RetryOverrides {
    pub enabled: Option<bool>,
    pub max_retries: Option<u32>,
    pub base_delay_ms: Option<u64>,
    pub max_agent_delay_ms: Option<u64>,
}

/// Partial compaction policy of [`HarnessSettings`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CompactionOverrides {
    pub enabled: Option<bool>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub background_tokens: Option<u64>,
}

/// Partial progress policy of [`HarnessSettings`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProgressOverrides {
    pub partial_interval_ms: Option<u64>,
    pub output_interval_ms: Option<u64>,
}

/// Harness-wide run policy. `HarnessOptions::settings` returns it at every resolution.
#[derive(Clone, Default)]
pub struct HarnessSettings {
    /// Default extension selection; `None`: every installed extension, in install order.
    pub extensions: Option<Vec<Arc<Extension>>>,
    pub stream: Option<ConversationStreamOptions>,
    pub retry: Option<RetryOverrides>,
    pub compaction: Option<CompactionOverrides>,
    pub progress: Option<ProgressOverrides>,
    pub tool_execution: Option<ToolExecutionMode>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    /// How long an idle conversation keeps its last context read in memory; `0` drops it at once.
    pub context_retention_ms: Option<f64>,
}

/// Resolved settings: every field over its built-in default.
#[derive(Clone)]
pub struct Settings {
    pub extensions: Option<Vec<Arc<Extension>>>,
    pub stream: ConversationStreamOptions,
    pub retry: ConversationRetryPolicy,
    pub compaction: CompactionPolicy,
    pub progress: ProgressPolicy,
    pub tool_execution: ToolExecutionMode,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub context_retention_ms: f64,
}

impl Default for Settings {
    fn default() -> Self {
        resolve_settings(None)
    }
}

/// Where the Harness reads its settings; called at each access.
pub type SettingsSource = Arc<dyn Fn() -> DurableResult<HarnessSettings> + Send + Sync>;

/// A settings source that always returns `settings`.
pub fn fixed_settings(settings: HarnessSettings) -> SettingsSource {
    Arc::new(move || Ok(settings.clone()))
}

/// Resolve the host settings: every field over its built-in default, object fields merged field by field.
pub fn resolve_settings(settings: Option<&HarnessSettings>) -> Settings {
    let default = HarnessSettings::default();
    let settings = settings.unwrap_or(&default);
    let retry = settings.retry.unwrap_or_default();
    let compaction = settings.compaction.unwrap_or_default();
    let progress = settings.progress.unwrap_or_default();
    Settings {
        extensions: settings.extensions.clone(),
        stream: settings.stream.clone().unwrap_or_default(),
        retry: ConversationRetryPolicy {
            enabled: retry.enabled.unwrap_or(DEFAULT_RETRY_POLICY.enabled),
            max_retries: retry.max_retries.unwrap_or(DEFAULT_RETRY_POLICY.max_retries),
            base_delay_ms: retry.base_delay_ms.unwrap_or(DEFAULT_RETRY_POLICY.base_delay_ms),
            max_agent_delay_ms: retry.max_agent_delay_ms.or(DEFAULT_RETRY_POLICY.max_agent_delay_ms),
        },
        compaction: CompactionPolicy {
            enabled: compaction.enabled.unwrap_or(DEFAULT_COMPACTION_POLICY.enabled),
            reserve_tokens: compaction.reserve_tokens.unwrap_or(DEFAULT_COMPACTION_POLICY.reserve_tokens),
            keep_recent_tokens: compaction.keep_recent_tokens.unwrap_or(DEFAULT_COMPACTION_POLICY.keep_recent_tokens),
            background_tokens: compaction.background_tokens.unwrap_or(DEFAULT_COMPACTION_POLICY.background_tokens),
        },
        progress: ProgressPolicy {
            partial_interval_ms: progress.partial_interval_ms.unwrap_or(DEFAULT_PROGRESS_POLICY.partial_interval_ms),
            output_interval_ms: progress.output_interval_ms.unwrap_or(DEFAULT_PROGRESS_POLICY.output_interval_ms),
        },
        tool_execution: settings.tool_execution.unwrap_or_default(),
        steering_mode: settings.steering_mode.unwrap_or_default(),
        follow_up_mode: settings.follow_up_mode.unwrap_or_default(),
        context_retention_ms: settings.context_retention_ms.unwrap_or(DEFAULT_CONTEXT_RETENTION_MS),
    }
}
