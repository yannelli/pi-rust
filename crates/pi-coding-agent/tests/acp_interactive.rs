//! The ACP controls of the interactive mode, in their own test binary: ACP agents and the ACP host are
//! process-wide, and the other interactive tests reset them when they create services.

#[path = "interactive/acp_controls.rs"]
mod acp_controls;
#[path = "interactive/harness.rs"]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;
