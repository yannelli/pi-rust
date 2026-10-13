//! RPC mode: in-process over the faux provider, through the `pi` binary with a local mock
//! server, and the `RpcClient` (upstream `rpc*.test.ts`).

#[path = "../cli_support/mod.rs"]
mod cli_support;
#[allow(dead_code)]
#[path = "../agent_session/harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "../agent_session/mcp_fake.rs"]
mod mcp_fake;

mod binary;
mod builtins;
mod client;
mod extension_host;
mod extension_ui;
mod framing;
mod model;
mod prompt;
mod session;
mod support;
