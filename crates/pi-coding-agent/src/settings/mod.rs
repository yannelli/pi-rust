//! Settings from `~/.pi/agent/settings.json` and `.pi/settings.json`, with the upstream
//! schema, merge rules, defaults, diagnostics, and lock-protected writes.

mod accessors;
mod accessors_ui;
pub mod defaults;
mod manager;
pub mod schema;
mod storage;
pub mod tools;

pub use accessors::{
    CompactionConfig, ProviderRetryConfig, QuietStartup, ResourceKind, RetryConfig, parse_timeout_setting,
};
pub use defaults::DEFAULT_THINKING_LEVEL;
pub use manager::{SettingsError, SettingsManager, SettingsOptions};
pub use schema::Settings;
pub use storage::{FileSettingsStorage, InMemorySettingsStorage, SettingsEdit, SettingsScope, SettingsStorage};

/// Global provider exclusions, including for catalog-only CLI commands.
pub(crate) fn disabled_providers(agent_dir: &std::path::Path) -> Vec<String> {
    SettingsManager::create(agent_dir, agent_dir, SettingsOptions { project_trusted: false }).disabled_providers()
}

#[cfg(test)]
mod tests;
