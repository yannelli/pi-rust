//! The integration tests, built as one test binary; worker_admission.rs has its own (see Cargo.toml).

mod common;
mod support;

mod codemode_models;
mod codemode_tool;
mod declarations;
mod deterministic;
mod loadout;
mod sandbox;
mod sandbox_limits;
mod source;
mod tool_search;
