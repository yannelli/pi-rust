//! The built-in Antigravity controls of the interactive mode, in their own test binary: the Antigravity
//! sessions and the fake server they start are process-wide.

// The fake server starts through a shell script.
#[cfg(unix)]
#[path = "interactive/antigravity_controls.rs"]
mod antigravity_controls;
#[path = "interactive/harness.rs"]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;
