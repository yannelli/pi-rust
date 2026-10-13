//! In-process pi sessions for native and Rust applications. Port of upstream `pi-embedded`.
//! Applications own model requests, tools, credentials, workspace access, approval,
//! checkpoints, and lifecycle decisions; the core runs pi's agent loop over them.

mod compact;
mod credentials;
mod execute;
pub mod ffi;
mod fs_store;
mod fs_util;
mod fs_workspace;
mod host;
mod http;
mod memory;
mod native;
mod native_host;
mod openai;
mod openai_payload;
mod run;
mod runtime;
mod session;
mod types;
mod workspace;

pub use credentials::EnvironmentCredentials;
pub use fs_store::FileSessionStore;
pub use fs_workspace::FsWorkspace;
pub use host::{
    Compactor, CredentialStore, EmbeddedHost, EmbeddedTool, FnTool, Lifecycle, LifecycleListener, ListenerErrorFn,
    NetworkAdapter, NetworkResponse, ProgressFn, SessionStore, ToolContext, ToolDeclaration, WorkspaceAdapter,
};
pub use http::HttpNetworkAdapter;
pub use memory::MemorySessionStore;
pub use native::{NativeBridge, NativeHost};
pub use openai::{create_model_compactor, create_openai_stream, empty_usage};
pub use runtime::{CreateSessionOptions, EmbeddedRuntime};
pub use session::{EmbeddedSession, SessionListener, Subscription};
pub use types::*;
pub use workspace::create_workspace_tools;
