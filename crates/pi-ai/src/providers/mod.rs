//! Built-in wire protocols.

pub mod anthropic;
pub mod aws;
pub mod bedrock;
mod common;
pub mod constrained_sampling;
pub mod faux;
pub mod github_copilot;
#[cfg(test)]
mod http_mock;
#[cfg(test)]
mod mock_server;
pub mod openai_completions;
pub mod openai_responses;
mod provider_env;
