//! Agent session integration tests over the faux provider.

mod bash;
mod cache;
mod catalog;
mod codemode;
mod codemode_store;
mod compaction;
pub(crate) mod harness;
pub(crate) mod http_mock;
mod llama;
mod login;
mod mcp;
mod mcp_discovery;
pub(crate) mod mcp_fake;
mod mcp_identity;
mod offline;
mod prompt;
mod retry;
mod run_limits;
mod runtime;
mod tool_loadout;
mod tool_options;
mod tool_registry;
mod tool_search;
mod tool_support;
mod tree;
mod virtual_models;
