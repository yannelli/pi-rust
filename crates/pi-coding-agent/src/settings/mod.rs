//! Settings from `~/.pi/agent/settings.json` and `.pi/settings.json`, with the upstream
//! schema, merge rules, defaults, diagnostics, and lock-protected writes.

mod accessors;
mod accessors_ui;
pub mod defaults;
mod deferred;
mod deferred_ops;
mod js;
mod js_text;
mod json_error;
mod manager;
pub mod schema;
mod sdk_alias;
mod sdk_members;
mod sdk_reads;
mod sdk_setters;
mod storage;
pub mod tools;

pub use accessors::{
    CompactionConfig, ProviderRetryConfig, QuietStartup, ResourceKind, RetryConfig, parse_timeout_setting,
};
pub use defaults::DEFAULT_THINKING_LEVEL;
pub(crate) use deferred::{HostStorage, Read, Thrown};
pub use manager::{SettingsError, SettingsManager, SettingsOptions};
pub use schema::Settings;
pub use storage::{FileSettingsStorage, InMemorySettingsStorage, SettingsEdit, SettingsScope, SettingsStorage};

/// Global provider exclusions, including for catalog-only CLI commands.
pub(crate) fn disabled_providers(agent_dir: &std::path::Path) -> Vec<String> {
    SettingsManager::create(agent_dir, agent_dir, SettingsOptions { project_trusted: false }).disabled_providers()
}

#[cfg(test)]
mod sdk_tests;
#[cfg(test)]
mod tests;
