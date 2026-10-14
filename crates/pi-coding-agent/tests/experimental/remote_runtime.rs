//! Port of the server and coordinator parts of upstream `test/experimental-remote-runtime.test.ts`. Cases
//! that attach Sessions are in `remote_runtime_workers.rs`. Rust tests cannot stub process environment
//! variables (that needs unsafe code), so options name the directory, server ID, and Session directory.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use pi_chord::BACKGROUND_CONTEXT;

use pi_coding_agent::experimental::cli::ClientCommand;
use pi_coding_agent::experimental::client::{ClientResult, SessionAddress, run_client};
use pi_coding_agent::experimental::client_runtime::{
    OpenClientRuntimeOptions, activate_builtin_client_services, open_client_runtime,
};
use pi_coding_agent::experimental::plugins::bundled::create_presentation_facet_loaders;
use pi_coding_agent::experimental::server_runtime::{StartServerOptions, start_server};
use pi_coding_agent::experimental::services::plugins::{PrepareSessionRequest, PresentationPlugins};
use pi_coding_agent::experimental::services::sessions::SessionManagement;

use super::remote_support::{
    Fixture, client_options, create_experimental_sessions, fixture, list_sessions, path_exists, temp_dir, wait_until,
};

const FIRST_ID: &str = "00000000-0000-4000-8000-000000000001";
const SECOND_ID: &str = "00000000-0000-4000-8000-000000000002";

fn pairs(server_id: &str, ids: &[&str]) -> Vec<(String, String)> {
    ids.iter().map(|id| (server_id.to_string(), id.to_string())).collect()
}

#[tokio::test]
async fn uses_the_server_directory_and_server_id() {
    let fixture = fixture().await;
    let directory = temp_dir("pi-server-dir-");
    let mut options = fixture.options(directory.path());
    options.server_id = Some(FIRST_ID.into());
    let runtime = start_server(options).await.unwrap();

    assert_eq!(runtime.server_id, FIRST_ID);
    assert_eq!(runtime.socket_path, directory.path().join(format!("{FIRST_ID}.sock")));
    let mode = |path: &std::path::Path| std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(directory.path()), 0o700);
    let control = directory.path().join(format!("control-{FIRST_ID}.sock"));
    for socket in [&runtime.socket_path, &control] {
        assert!(std::fs::symlink_metadata(socket).unwrap().file_type().is_socket());
        assert_eq!(mode(socket), 0o600);
    }
    let entries: Vec<String> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert!(entries.iter().all(|entry| !entry.starts_with('.')));
    assert!(entries.contains(&format!("{FIRST_ID}.sock")));
    assert!(entries.contains(&format!("control-{FIRST_ID}.sock")));
    let server = regex::Regex::new(&format!("^server-{FIRST_ID}-[0-9a-f]{{12}}\\.sock$")).unwrap();
    assert!(entries.iter().any(|entry| server.is_match(entry)));
    assert_eq!(list_sessions(directory.path()).await, pairs(FIRST_ID, &["demo-1", "demo-2"]));
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn rejects_a_provider_without_a_model() {
    let directory = temp_dir("pes-");
    let options = StartServerOptions {
        directory: Some(directory.path().display().to_string()),
        provider: Some("anthropic".into()),
        ..StartServerOptions::default()
    };
    let error = start_server(options).await.err().unwrap();
    assert!(error.to_string().contains("provider requires a model"));
}

// "preserves an existing Session model when the server default changes" needs the coding agent Session
// worker; it is in `remote_runtime_models.rs`.

#[tokio::test]
async fn rejects_model_options_when_discovery_selects_an_existing_server() {
    let fixture = fixture().await;
    let (directory, runtime) = fixture.make_server().await;
    let command = ClientCommand { model: Some("anthropic/claude-opus-4-6".into()), ..ClientCommand::default() };
    let error = run_client(&command, client_options(directory.path())).await.unwrap_err();
    assert!(error.to_string().contains("Model selection is only valid when automatically activating a new server"));
    runtime.close().await.unwrap();
}

// "rechecks an auto-discovered server after a version mismatch" mocks `Client.connect`, which Rust cannot
// replace inside `open_client_runtime`; `connect_route` activates the server anew on a `version` error.

#[tokio::test]
async fn serializes_concurrent_cold_activation_and_retires_after_both_clients_leave() {
    let fixture = fixture().await;
    let directory = temp_dir("pi-auto-server-");
    let options = OpenClientRuntimeOptions {
        requested_server_id: Some(FIRST_ID.into()),
        session_dir: Some(fixture.sessions.display().to_string()),
        ..client_options(directory.path())
    };
    let command = ClientCommand::default();
    let (first, second) = tokio::join!(run_client(&command, options.clone()), run_client(&command, options.clone()));
    let sessions: Vec<SessionAddress> =
        ["demo-1", "demo-2"].map(|id| SessionAddress { server_id: FIRST_ID.into(), session_id: id.into() }).into();
    assert_eq!(first.unwrap(), ClientResult::List { sessions: sessions.clone() });
    assert_eq!(second.unwrap(), ClientResult::List { sessions });
    let socket = directory.path().join(format!("{FIRST_ID}.sock"));
    let control = directory.path().join(format!("control-{FIRST_ID}.sock"));
    assert!(path_exists(&socket));
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&socket) }).await);
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&control) }).await);
}

fn cold_options(fixture: &Fixture, directory: &Path) -> OpenClientRuntimeOptions {
    OpenClientRuntimeOptions {
        requested_server_id: Some(FIRST_ID.into()),
        session_dir: Some(fixture.sessions.display().to_string()),
        ..client_options(directory)
    }
}

fn session_worker_model() -> ClientCommand {
    ClientCommand { provider: Some("anthropic".into()), model: Some("claude-sonnet-4-5".into()), ..Default::default() }
}

#[tokio::test]
async fn passes_client_plugin_packages_to_a_cold_server_and_restores_them_for_its_next_generation() {
    let fixture = fixture().await;
    let directory = temp_dir("pi-auto-plugin-");
    let package = env!("CARGO_MANIFEST_DIR").to_string() + "/examples/plugins/pi-example-plugin";
    let first = open_client_runtime(&session_worker_model(), cold_options(&fixture, directory.path())).await.unwrap();
    let activated = activate_builtin_client_services(&first.servers[0]).await.unwrap();
    let request = PrepareSessionRequest { session_id: "demo-1".into(), package_paths: Some(vec![package]) };
    let data = activated.plugins.prepare_session(request, &BACKGROUND_CONTEXT).await.unwrap();
    activated.management.attach("demo-1".into(), &BACKGROUND_CONTEXT).await.unwrap();
    let loaded = create_presentation_facet_loaders(&data).unwrap()[0].load().await.unwrap();
    let ids: Vec<&str> = loaded.facets.iter().map(|facet| facet.id()).collect();
    assert_eq!(ids, ["@earendil-works/pi-example-plugin/tui"]);
    loaded.dispose().await.unwrap();
    first.dispose().await.unwrap();
    let socket = directory.path().join(format!("{FIRST_ID}.sock"));
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&socket) }).await);

    let second = open_client_runtime(&session_worker_model(), cold_options(&fixture, directory.path())).await.unwrap();
    let activated = activate_builtin_client_services(&second.servers[0]).await.unwrap();
    let request = PrepareSessionRequest { session_id: "demo-1".into(), package_paths: None };
    let data = activated.plugins.prepare_session(request, &BACKGROUND_CONTEXT).await.unwrap();
    activated.management.attach("demo-1".into(), &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(create_presentation_facet_loaders(&data).unwrap().len(), 1);
    second.dispose().await.unwrap();
}

#[tokio::test]
async fn retires_a_cold_server_after_its_only_session_attachment_disconnects() {
    let fixture = fixture().await;
    let directory = temp_dir("pi-auto-session-");
    let command = ClientCommand { session_id: Some("demo-1".into()), ..session_worker_model() };
    let result = run_client(&command, cold_options(&fixture, directory.path())).await.unwrap();
    let expected = ClientResult::Attached { server_id: FIRST_ID.into(), session_id: "demo-1".into() };
    assert_eq!(result, expected);
    let socket = directory.path().join(format!("{FIRST_ID}.sock"));
    let control = directory.path().join(format!("control-{FIRST_ID}.sock"));
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&socket) }).await);
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&control) }).await);
}

#[tokio::test]
async fn runs_and_discovers_multiple_logical_servers_from_one_directory() {
    let fixture = fixture().await;
    let directory = temp_dir("pi-multi-server-");
    let options =
        |server_id: &str| StartServerOptions { server_id: Some(server_id.into()), ..fixture.options(directory.path()) };
    let (first, second) = tokio::join!(start_server(options(FIRST_ID)), start_server(options(SECOND_ID)));
    let (first, second) = (first.unwrap(), second.unwrap());
    for id in [FIRST_ID, SECOND_ID] {
        let control = std::fs::symlink_metadata(directory.path().join(format!("control-{id}.sock"))).unwrap();
        assert!(control.file_type().is_socket());
    }
    let discovered =
        pi_client::unix::discover_unix_servers(pi_client::unix::DiscoverUnixServersOptions::new(directory.path()));
    let mut routes = discovered.await.unwrap();
    routes.sort_by(|left, right| left.server_id.cmp(&right.server_id));
    assert_eq!(routes.len(), 2);
    let listed = list_sessions(directory.path()).await;
    let mut expected = pairs(FIRST_ID, &["demo-1", "demo-2"]);
    expected.extend(pairs(SECOND_ID, &["demo-1", "demo-2"]));
    assert_eq!(listed, expected);

    first.close().await.unwrap();
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&first.socket_path) }).await);
    let routes =
        pi_client::unix::discover_unix_servers(pi_client::unix::DiscoverUnixServersOptions::new(directory.path()));
    let routes: Vec<String> = routes.await.unwrap().into_iter().map(|route| route.server_id).collect();
    assert_eq!(routes, [SECOND_ID]);
    assert_eq!(list_sessions(directory.path()).await, pairs(SECOND_ID, &["demo-1", "demo-2"]));
    second.close().await.unwrap();
}

#[tokio::test]
async fn reports_missing_and_ambiguous_session_selections() {
    let fixture = fixture().await;
    let directory = temp_dir("ped-");
    let options =
        |server_id: &str| StartServerOptions { server_id: Some(server_id.into()), ..fixture.options(directory.path()) };
    let first = start_server(options(FIRST_ID)).await.unwrap();
    let second = start_server(options(SECOND_ID)).await.unwrap();
    let select = |session_id: &str| ClientCommand { session_id: Some(session_id.into()), ..ClientCommand::default() };
    let missing = run_client(&select("missing"), client_options(directory.path())).await.unwrap_err();
    assert!(missing.to_string().contains("No discovered server contains session missing"), "{missing}");
    let ambiguous = run_client(&select("demo-1"), client_options(directory.path())).await.unwrap_err();
    assert!(ambiguous.to_string().contains("Session demo-1 is available from more than one server"), "{ambiguous}");
    first.close().await.unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
async fn rejects_a_duplicate_session_id_within_one_session_directory() {
    let fixture = fixture().await;
    let other = fixture.agent_dir.path().join("other-cwd").display().to_string();
    let error = create_experimental_sessions(&fixture.sessions, &["demo-1"], &other).await.err().unwrap();
    assert!(error.to_string().contains("Session demo-1 already exists"));
}
