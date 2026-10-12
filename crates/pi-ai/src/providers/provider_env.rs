//! Provider configuration lookups: request-scoped `StreamOptions.env` values win over
//! the process environment, as upstream `getProviderEnvValue` does.

use std::collections::BTreeMap;

/// The first non-empty value of `name` from `env`, then from the process environment.
pub(crate) fn provider_env_value(name: &str, env: &BTreeMap<String, String>) -> Option<String> {
    env.get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// Only the process environment, for checks upstream makes without scoped overrides.
pub(crate) fn process_env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The user's home directory from `HOME`, `USERPROFILE`, or `HOMEDRIVE` plus `HOMEPATH`.
pub(crate) fn home_dir(env: &BTreeMap<String, String>) -> Option<std::path::PathBuf> {
    if let Some(home) = provider_env_value("HOME", env).or_else(|| provider_env_value("USERPROFILE", env)) {
        return Some(home.into());
    }
    let drive = provider_env_value("HOMEDRIVE", env).unwrap_or_default();
    provider_env_value("HOMEPATH", env).map(|path| format!("{drive}{path}").into())
}
