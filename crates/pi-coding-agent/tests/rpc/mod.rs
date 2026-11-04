//! RPC mode: in-process over the faux provider, through the `pi` binary with a local mock
//! server, and the `RpcClient` (upstream `rpc*.test.ts`).

use crate::agent_session::{harness, mcp_fake};

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
