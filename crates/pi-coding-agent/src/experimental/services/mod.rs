//! Experimental client/server service slices (upstream `experimental/services`). Remote services are
//! traits: `*_service_impl` serves one through Chord, and the `*Client` types call one through a handle.

pub mod agent_controller;
mod codec;
#[cfg(unix)]
pub mod connection;
#[cfg(unix)]
mod connection_session;
pub mod models;
pub mod plugins;
pub mod presentation_ui;
pub mod server;
pub mod sessions;
pub mod slash_commands;
pub mod transcript;
pub mod worker;

pub use codec::{decode_arg, decode_result, encode};
