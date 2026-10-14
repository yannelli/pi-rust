//! The eval suites under `evals/`, their fixtures, and helpers the oracles share.

use std::future::Future;
use std::path::{Path, PathBuf};

use pi_coding_agent::agent_session::runtime::AgentSessionRuntime;
use pi_coding_agent::app::{AppOptions, create_app_runtime};
use pi_coding_agent::session::SessionManager;
use serde_json::Value;

use crate::framework::EvalSuite;

#[path = "../evals/acme-server.rs"]
pub mod acme_server;
#[path = "../evals/configured-runtime.rs"]
pub mod configured_runtime;
#[path = "../evals/custom-provider.docs.eval.rs"]
pub mod custom_provider_docs;
#[path = "../evals/documentation-audit.eval.rs"]
pub mod documentation_audit;
#[path = "../evals/extensions.docs.eval.rs"]
pub mod extensions_docs;
#[path = "../evals/models.docs.eval.rs"]
pub mod models_docs;
#[path = "../evals/openai-provider.docs.eval.rs"]
pub mod openai_provider_docs;
#[path = "../evals/smoke.eval.rs"]
pub mod smoke;
#[path = "../evals/tui.docs.eval.rs"]
pub mod tui_docs;
#[path = "../evals/tui-oracle.rs"]
pub mod tui_oracle;

/// Every suite, host and documentation.
pub fn all_suites() -> Vec<EvalSuite> {
    vec![
        custom_provider_docs::suite(),
        documentation_audit::suite(),
        extensions_docs::suite(),
        models_docs::suite(),
        openai_provider_docs::suite(),
        smoke::suite(),
        tui_docs::suite(),
    ]
}

/// The pi-rust repository root: `PI_EVAL_REPO_ROOT`, else the workspace this crate built from.
pub fn repository_root() -> PathBuf {
    match std::env::var_os("PI_EVAL_REPO_ROOT") {
        Some(root) => PathBuf::from(root),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
    }
}

/// The configured runtime of a finished run, as a reloaded session sees it.
pub async fn load_agent_runtime(agent_dir: &Path, workspace: &Path) -> anyhow::Result<AgentSessionRuntime> {
    let options = AppOptions { agent_dir: agent_dir.to_path_buf(), ..AppOptions::default() };
    create_app_runtime(options, SessionManager::in_memory(workspace)).await
}

/// Run an oracle on its own thread and single-threaded runtime, since agent runtimes and
/// the interactive mode are not `Send`.
pub async fn run_local<F, Fut>(oracle: F) -> anyhow::Result<Value>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<Value>> + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| runtime.block_on(tokio::task::LocalSet::new().run_until(oracle())));
        let _ = sender.send(result);
    });
    receiver.await.map_err(|_| anyhow::anyhow!("The oracle thread stopped without a result."))?
}
