//! RPC mode (upstream `modes/rpc/*`, `docs/rpc.md`, `docs/rpc-commands.md`, and
//! `docs/rpc-extension-ui.md`): Pi as a long-lived subprocess driven by JSONL commands on
//! stdin, with responses, session events, and extension UI requests on stdout.

mod builtins;
mod client;
mod client_api;
mod client_types;
mod commands;
mod host;
pub mod jsonl;
mod output;
mod server;
mod tree;
mod types;
mod ui;

pub use builtins::builtin_commands;
pub use client::{RpcClient, RpcClientOptions};
pub use client_api::collect_until_settled;
pub use client_types::{
    RpcClearedQueue, RpcCompactionResult, RpcContextUsage, RpcEntries, RpcForkMessage, RpcForkResult, RpcModelCycle,
    RpcSessionStats, RpcTokenTotals, RpcUiResponse,
};
pub use commands::{resource_commands, session_state};
pub use host::{ExtensionErrorRecord, RpcExtensionBinding, RpcExtensionHost, RpcShutdownHandle};
pub use output::{RawStdout, RpcOutput};
pub use server::{RpcModeOptions, run_rpc_mode, serve_rpc};
pub use tree::{RpcSessionTree, RpcTreeNode, parse_json_line, tree_data_json};
pub use types::{
    COMMAND_TYPES, InputRecord, RpcBashResult, RpcCommand, RpcCommandSource, RpcSessionState, RpcSlashCommand,
    RpcStreamingBehavior, parse_input,
};
pub use ui::RpcExtensionUi;
