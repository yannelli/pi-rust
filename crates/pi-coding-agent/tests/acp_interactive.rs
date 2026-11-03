//! The ACP controls of the interactive mode, each in a process of its own: ACP agents and the ACP host are
//! process-wide, and the other interactive tests reset them when they create services.

#[path = "interactive/acp_controls.rs"]
mod acp_controls;
