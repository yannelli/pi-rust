//! Built-in wire protocols.

pub mod anthropic;
mod common;
pub mod faux;
#[cfg(test)]
pub(crate) mod mock_server;
pub mod openai_completions;
pub mod openai_responses;
#[cfg(test)]
mod plumbing_tests;
mod transform;
