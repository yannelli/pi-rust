//! Built-in wire protocols.

pub mod anthropic;
pub mod aws;
pub mod bedrock;
pub mod cloudflare;
mod common;
pub mod constrained_sampling;
pub mod faux;
pub mod github_copilot;
#[cfg(test)]
mod http_mock;
#[cfg(test)]
pub(crate) mod mock_server;
pub mod openai_codex;
pub mod openai_completions;
pub mod openai_responses;
#[cfg(test)]
mod plumbing_tests;
mod provider_env;
mod transform;
