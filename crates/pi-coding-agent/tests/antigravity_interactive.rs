//! The built-in Antigravity controls of the interactive mode, in a child process: the Antigravity
//! sessions and the fake server they start are process-wide.

// The fake server starts through a shell script.
#[cfg(unix)]
#[path = "interactive/antigravity_controls.rs"]
mod antigravity_controls;
