//! Built-in wire protocols.

pub mod anthropic;
mod common;
mod emitter;
pub mod faux;
pub mod google;
pub mod google_vertex;
mod http_stream;
pub mod mistral;
#[cfg(test)]
mod mock_http;
#[cfg(test)]
pub(crate) mod mock_server;
pub mod openai_completions;
pub mod openai_responses;
pub mod pi_messages;
#[cfg(test)]
mod plumbing_tests;
mod strict_schema;
mod transform;
