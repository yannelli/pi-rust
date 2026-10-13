//! Port of upstream `test/env-node.test.ts`: the local execution environment's file system, text line reader, and
//! shell. Split into modules to keep files short.

#[path = "support/env.rs"]
mod support;

#[path = "env_node/filesystem.rs"]
mod filesystem;
#[path = "env_node/shell.rs"]
mod shell;
#[path = "env_node/spill.rs"]
mod spill;
#[path = "env_node/text_lines.rs"]
mod text_lines;
