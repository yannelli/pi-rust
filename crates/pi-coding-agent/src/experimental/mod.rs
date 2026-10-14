//! Experimental client/server runtime (upstream `src/experimental`). Behind `PI_EXPERIMENTAL=1`.
//! Upstream's `source-resolver.ts` is a Node module-resolution hook and has no Rust counterpart.

pub mod cli;
#[cfg(unix)]
pub mod client;
#[cfg(unix)]
pub mod client_runtime;
#[cfg(unix)]
pub mod client_tui;
#[cfg(unix)]
mod client_tui_chat;
#[cfg(unix)]
pub mod client_tui_parts;
#[cfg(unix)]
pub mod client_tui_run;
#[cfg(unix)]
mod client_tui_session;
#[cfg(all(test, unix))]
mod client_tui_tests;
#[cfg(unix)]
pub mod commands;
#[cfg(unix)]
pub mod control;
#[cfg(unix)]
pub mod coordinator;
#[cfg(unix)]
pub mod coordinator_process;
pub mod durable;
pub mod entry;
pub mod lockfile;
pub mod plugin;
pub mod plugins;
pub mod process;
#[cfg(unix)]
pub mod radius_auth;
#[cfg(unix)]
pub mod radius_client;
#[cfg(unix)]
pub mod radius_relay;
#[cfg(unix)]
mod radius_socket;
#[cfg(unix)]
pub mod relay;
#[cfg(unix)]
pub mod server;
#[cfg(unix)]
pub mod server_activation;
#[cfg(unix)]
pub mod server_backend;
#[cfg(unix)]
pub mod server_runtime;
pub mod services;
pub mod session_catalog;
pub mod session_worker;
#[cfg(unix)]
mod session_worker_attach;
#[cfg(unix)]
mod session_worker_events;
#[cfg(unix)]
pub mod session_worker_harness;
#[cfg(unix)]
pub mod session_worker_manager;
#[cfg(unix)]
pub mod session_worker_run;
pub mod vacation;
