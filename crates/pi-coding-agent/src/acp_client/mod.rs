//! ACP agents that pi runs as providers: the `acpAgents` settings, the ACP registry, and install.

pub mod entry;
pub mod host;
pub mod install;
pub mod providers;
pub mod register;
pub mod registry;
pub mod store;

#[cfg(test)]
mod tests;

pub use entry::{AcpAgentEntry, AcpScope, provider_id};
pub use registry::{REGISTRY_URL, RegistryAgent, builtin_agents, platform_key, registry_url, with_builtins};

#[cfg(test)]
mod register_tests;
