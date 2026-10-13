//! ACP mode, ported from pi-acp by Sergii Kozak (MIT; see `LICENSE-pi-acp`).
//! `pi --mode acp` speaks the Agent Client Protocol (JSON-RPC 2.0 over stdio) so editors such
//! as Zed can run pi as an agent server. Each ACP session drives an in-process pi session.

mod agent;
mod agent_load;
mod agent_prompt;
pub mod auth;
pub mod backend;
pub mod bash;
pub mod commands;
pub mod config;
pub mod connection;
pub mod errors;
mod events;
mod ext_ui;
pub mod listing;
pub mod runtime;
mod session;
pub mod settings;
pub mod startup;
pub mod store;
pub mod translate;

#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncRead;

pub use agent::{AcpAgent, AcpAgentOptions, Handled, embedded_context_enabled};
pub use agent_load::replay_message;
pub use agent_prompt::format_session_stats;
pub use backend::{AcpClient, BackendFactory, PiBackend};
pub use connection::ClientConnection;
pub use events::{edit_old_texts, find_unique_line_number, format_auto_retry_message, tool_call_locations};
pub use ext_ui::{extension_ui_tool_call, option_index};
pub use runtime::{EventEncoder, RuntimeBackend, RuntimeBackendFactory};
pub use session::{AcpSession, AcpSessionParams, SESSION_STATS_TIMEOUT, TurnEnd, TurnResult, to_usage_update};

use crate::app::AppOptions;
use crate::cli::Args;
use crate::config::{ENV_SESSION_DIR, expand_tilde, home_dir};
use crate::rpc::{RawStdout, RpcOutput};

/// Options of [`serve_acp`].
#[derive(Clone)]
pub struct AcpModeOptions {
    /// What every session the agent opens is created with.
    pub app: AppOptions,
    /// The session directory override for new sessions.
    pub session_dir: Option<PathBuf>,
    pub agent: AcpAgentOptions,
    /// Stop on SIGINT and SIGTERM (Unix).
    pub handle_signals: bool,
    /// Check for a newer pi release for the startup info.
    pub version_check: bool,
    /// Extensions, bound to every session with ACP dialogs (see `RpcExtensionHost`).
    pub extension_host: Option<Arc<dyn crate::rpc::RpcExtensionHost>>,
}

impl AcpModeOptions {
    /// Options for `app`, with the session directory from `--session-dir`,
    /// `PI_CODING_AGENT_SESSION_DIR`, or the `sessionDir` setting.
    pub fn new(app: AppOptions, session_dir_arg: Option<&str>) -> Self {
        let agent_dir = app.agent_dir.clone();
        let env_dir = pi_ai::env::process_env_value(ENV_SESSION_DIR).filter(|dir| !dir.is_empty());
        let session_dir = session_dir_arg
            .or(env_dir.as_deref())
            .map(expand_tilde)
            .or_else(|| settings::settings_session_dir(&agent_dir));
        let agent = AcpAgentOptions {
            sessions_root: settings::sessions_root(&agent_dir, session_dir.as_deref()),
            agent_dir,
            home: home_dir(),
            session_map_path: store::default_session_map_path(),
            embedded_context: embedded_context_enabled(std::env::var("PI_ACP_ENABLE_EMBEDDED_CONTEXT").ok().as_deref()),
            terminal_launch: auth::default_launch(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        Self { app, session_dir, agent, handle_signals: false, version_check: false, extension_host: None }
    }

    /// Options from the command line, as `pi --mode acp` uses them.
    pub fn from_args(args: &Args, cwd: &Path, agent_dir: &Path, oauth: pi_ai::auth::OAuthLookup) -> Self {
        let app = AppOptions { oauth: Some(oauth), ..AppOptions::from_args(args, cwd, agent_dir) };
        Self { handle_signals: true, version_check: true, ..Self::new(app, args.session_dir.as_deref()) }
    }
}

#[cfg(unix)]
async fn termination_signal() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => 0,
        _ = interrupt.recv() => 0,
    }
}

#[cfg(not(unix))]
async fn termination_signal() -> i32 {
    let _ = tokio::signal::ctrl_c().await;
    0
}

/// Serve ACP over `input` and `output` until the client disconnects; returns the exit code.
pub async fn serve_acp(options: AcpModeOptions, input: impl AsyncRead + Unpin, output: Box<dyn Write + Send>) -> i32 {
    let output = RpcOutput::spawn(output);
    let client = ClientConnection::new(output.clone());
    let factory = Arc::new(RuntimeBackendFactory {
        app: options.app,
        session_dir: options.session_dir,
        extension_host: options.extension_host,
    });
    let check = if options.version_check { crate::startup::spawn_version_check() } else { None };
    let agent = AcpAgent::new(client.clone(), factory, options.agent, check);
    let handle_signals = options.handle_signals;
    let stop = async move { if handle_signals { termination_signal().await } else { std::future::pending().await } };
    let code = connection::read_loop(&agent, &client, input, stop).await;
    agent.dispose().await;
    output.drained().await;
    code
}

/// `pi --mode acp`: reserve stdout for protocol records and serve stdin.
pub async fn run_acp_mode(options: AcpModeOptions) -> i32 {
    if let Err(failure) = crate::diagnostics::take_over_stdout() {
        eprintln!("Error: could not reserve stdout for ACP mode: {failure}");
        return 1;
    }
    serve_acp(options, tokio::io::stdin(), Box::new(RawStdout)).await
}
