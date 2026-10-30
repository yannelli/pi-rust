//! Built-in wire protocols.

pub mod anthropic;
pub mod antigravity_acp;
pub mod aws;
pub mod azure_openai_responses;
pub mod bedrock;
pub mod cloudflare;
mod common;
pub mod constrained_sampling;
pub mod cursor;
mod emitter;
pub mod faux;
pub mod github_copilot;
pub mod google;
pub mod google_vertex;
pub mod grok_build;
#[cfg(test)]
pub(crate) mod http_mock;
mod http_stream;
pub mod mistral;
#[cfg(test)]
mod mock_http;
#[cfg(test)]
pub(crate) mod mock_server;
pub mod openai_codex;
pub mod openai_completions;
pub mod openai_responses;
mod openrouter_cost;
pub mod pi_messages;
#[cfg(test)]
mod plumbing_tests;
mod provider_env;
#[cfg(test)]
mod streaming_arguments_tests;
mod transform;
