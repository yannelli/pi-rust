//! Built-in wire protocols.

pub mod anthropic;
mod common;
pub mod faux;
#[cfg(test)]
mod mock_server;
pub mod openai_completions;
pub mod openai_responses;
mod transform;
