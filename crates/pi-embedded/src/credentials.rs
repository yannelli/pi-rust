//! Credentials from environment variables (upstream Node `EnvironmentCredentials`).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::host::CredentialStore;
use crate::runtime::lock;

/// Maps providers to environment variable names (default `openai` -> `OPENAI_API_KEY`).
/// `set` records values in this store only: changing the process environment is unsafe in
/// multithreaded Rust, so the store keeps its own overrides over the process values.
pub struct EnvironmentCredentials {
    variables: HashMap<String, String>,
    /// `Some(None)` marks a variable cleared through `set`.
    overrides: Mutex<HashMap<String, Option<String>>>,
    process: bool,
}

impl Default for EnvironmentCredentials {
    fn default() -> Self {
        Self::new(HashMap::from([("openai".to_string(), "OPENAI_API_KEY".to_string())]))
    }
}

impl EnvironmentCredentials {
    /// Read variables from the process environment.
    pub fn new(variables: HashMap<String, String>) -> Self {
        Self { variables, overrides: Mutex::default(), process: true }
    }

    /// Read variables only from `environment`, never from the process.
    pub fn with_environment(variables: HashMap<String, String>, environment: HashMap<String, String>) -> Self {
        let overrides = environment.into_iter().map(|(name, value)| (name, Some(value))).collect();
        Self { variables, overrides: Mutex::new(overrides), process: false }
    }

    /// The current value of an environment variable as this store sees it.
    pub fn variable(&self, name: &str) -> Option<String> {
        match lock(&self.overrides).get(name) {
            Some(value) => value.clone(),
            None if self.process => std::env::var(name).ok(),
            None => None,
        }
    }
}

#[async_trait]
impl CredentialStore for EnvironmentCredentials {
    async fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
        Ok(self.variables.get(provider).and_then(|variable| self.variable(variable)))
    }

    async fn set(&self, provider: &str, secret: Option<String>) -> anyhow::Result<()> {
        let Some(variable) = self.variables.get(provider) else {
            anyhow::bail!("No credential environment variable configured: {provider}");
        };
        lock(&self.overrides).insert(variable.clone(), secret);
        Ok(())
    }
}
