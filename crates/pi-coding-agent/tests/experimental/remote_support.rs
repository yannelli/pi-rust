//! Shared setup for the remote runtime tests (upstream `experimental-remote-runtime.test.ts` helpers and
//! `experimental-session-support.ts`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use pi_chord::BACKGROUND_CONTEXT;
use pi_client::unix::{UnixTransportOptions, create_unix_transport_factory};
use pi_client::{Client, ClientOptions};
use pi_coding_agent::experimental::cli::ClientCommand;
use pi_coding_agent::experimental::client::{ClientResult, run_client};
use pi_coding_agent::experimental::client_runtime::OpenClientRuntimeOptions;
use pi_coding_agent::experimental::server_runtime::{RunningServer, StartServerOptions, start_server};
use pi_coding_agent::experimental::services::sessions::{
    SESSION_MANAGEMENT, SessionManagement, SessionManagementClient,
};
use pi_coding_agent::experimental::session_catalog::{SessionCatalogMetadata, create_session};

use super::faux_worker;
use super::service_binding::{Errors, create_server_service_binding};

pub fn temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir_in("/tmp").unwrap()
}

/// Upstream `createExperimentalSessions`.
pub async fn create_experimental_sessions(
    root: &Path,
    ids: &[&str],
    cwd: &str,
) -> pi_chord::ChordResult<Vec<SessionCatalogMetadata>> {
    let mut sessions = Vec::new();
    for id in ids {
        sessions.push(create_session(root, Some(id), cwd).await?);
    }
    Ok(sessions)
}

/// The agent directory with Sessions `demo-1` and `demo-2`. Rust tests pass it explicitly: they cannot
/// stub `PI_CODING_AGENT_DIR` without unsafe code.
pub struct Fixture {
    pub agent_dir: tempfile::TempDir,
    pub sessions: PathBuf,
}

pub async fn fixture() -> Fixture {
    faux_worker::install();
    let agent_dir = temp_dir("pi-experimental-agent-");
    let sessions = agent_dir.path().join("experimental").join("sessions");
    let cwd = std::env::current_dir().unwrap().display().to_string();
    create_experimental_sessions(&sessions, &["demo-1", "demo-2"], &cwd).await.unwrap();
    Fixture { agent_dir, sessions }
}

impl Fixture {
    pub fn options(&self, directory: &Path) -> StartServerOptions {
        StartServerOptions {
            directory: Some(directory.display().to_string()),
            session_dir: Some(self.sessions.display().to_string()),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-5".into()),
            ..StartServerOptions::default()
        }
    }

    pub async fn make_server(&self) -> (tempfile::TempDir, RunningServer) {
        let directory = temp_dir("pes-");
        let runtime = start_server(self.options(directory.path())).await.unwrap();
        (directory, runtime)
    }
}

pub async fn connect(server_id: &str, socket_path: &Path) -> Client {
    let factory = create_unix_transport_factory(UnixTransportOptions::new(socket_path)).unwrap();
    let client = Client::new(ClientOptions::new(server_id, factory)).unwrap();
    client.connect().await.unwrap();
    client
}

pub async fn attach_session(client: &Client, session_id: &str) -> pi_chord::ChordResult<()> {
    let errors = Errors::default();
    let services = create_server_service_binding(client, &[&SESSION_MANAGEMENT], &errors);
    let management = SessionManagementClient(services.use_service(&SESSION_MANAGEMENT));
    let result = async {
        services.ready(&BACKGROUND_CONTEXT).await?;
        management.attach(session_id.into(), &BACKGROUND_CONTEXT).await
    }
    .await;
    services.dispose(&BACKGROUND_CONTEXT).await?;
    result
}

pub async fn attach_client(runtime: &RunningServer, session_id: &str) -> Client {
    let client = connect(&runtime.server_id, &runtime.socket_path).await;
    attach_session(&client, session_id).await.unwrap();
    client
}

/// Client options for a server directory; Rust tests name it, as they cannot stub `PI_SERVER_DIR`.
pub fn client_options(directory: &Path) -> OpenClientRuntimeOptions {
    OpenClientRuntimeOptions { directory: Some(directory.display().to_string()), ..OpenClientRuntimeOptions::default() }
}

/// The `(serverId, sessionId)` pairs `runClient` lists for the servers it discovers in `directory`.
pub async fn list_sessions(directory: &Path) -> Vec<(String, String)> {
    match run_client(&ClientCommand::default(), client_options(directory)).await.unwrap() {
        ClientResult::List { sessions } => {
            sessions.into_iter().map(|session| (session.server_id, session.session_id)).collect()
        }
        other => panic!("expected a Session list, got {other:?}"),
    }
}

pub async fn wait_until<F, Fut>(timeout: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
