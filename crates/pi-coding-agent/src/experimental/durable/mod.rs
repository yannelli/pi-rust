//! Port of upstream `durable/`: a small local coding agent on the durable Harness. One process owns the model
//! runtime, the Harness, its SQLite storage, and the TUI. `vacation/` reuses the runtime and the TUI.

pub mod controller;
pub mod harness_setup;
pub mod open;
pub mod prompt;
pub mod runtime;
pub mod sessions;
pub mod subagent;
mod tui;
pub(crate) mod tui_parts;
mod tui_run;
mod tui_sync;
#[cfg(test)]
mod tui_tests;

pub use controller::DurableController;
pub use open::{DurableApp, OpenDurableOptions, OpenDurableResult, open_durable, parse_durable_args};
pub use runtime::{DurableView, ViewStore, agent_of};
pub use tui_run::{run_durable_main, run_durable_tui};
