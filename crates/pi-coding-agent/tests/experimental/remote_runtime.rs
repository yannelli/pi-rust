//! Port of the server and coordinator parts of upstream `test/experimental-remote-runtime.test.ts`. Cases
//! that attach Sessions are in `remote_runtime_workers.rs`. Rust tests cannot stub process environment
//! variables (that needs unsafe code), so options name the directory, server ID, and Session directory.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::time::Duration;

use pi_coding_agent::experimental::server_activation::{ActivateServerOptions, activate_server};
use pi_coding_agent::experimental::server_runtime::{StartServerOptions, start_server};

use super::remote_support::{create_experimental_sessions, fixture, list_sessions, path_exists, temp_dir, wait_until};

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
    // Upstream lists through `runClient` (step 3); the server's Session directory is what it reads.
    assert_eq!(list_sessions(&runtime.server_id, &runtime.socket_path).await, pairs(FIRST_ID, &["demo-1", "demo-2"]));
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
    let error = activate_server(ActivateServerOptions {
        directory: directory.path().to_path_buf(),
        requested_server_id: None,
        session_dir: fixture.sessions.clone(),
        provider: Some("anthropic".into()),
        model: Some("claude-opus-4-6".into()),
    })
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("Model selection is only valid when automatically activating a new server"));
    runtime.close().await.unwrap();
}

// "rechecks an auto-discovered server after a version mismatch" mocks `Client.connect` inside
// `openClientRuntime`, which is client runtime code (step 3).

#[tokio::test]
async fn serializes_concurrent_cold_activation_and_retires_after_both_clients_leave() {
    let fixture = fixture().await;
    let directory = temp_dir("pi-auto-server-");
    let activate = || {
        activate_server(ActivateServerOptions {
            directory: directory.path().to_path_buf(),
            requested_server_id: Some(FIRST_ID.into()),
            session_dir: fixture.sessions.clone(),
            provider: None,
            model: None,
        })
    };
    let (first, second) = tokio::join!(activate(), activate());
    let (first, second) = (first.unwrap(), second.unwrap());
    for activated in [&first, &second] {
        assert_eq!(activated.route.server_id, FIRST_ID);
        let listed = list_sessions(FIRST_ID, &activated.route.path).await;
        assert_eq!(listed, pairs(FIRST_ID, &["demo-1", "demo-2"]));
    }
    let socket = directory.path().join(format!("{FIRST_ID}.sock"));
    let control = directory.path().join(format!("control-{FIRST_ID}.sock"));
    assert!(path_exists(&socket));
    first.client.dispose();
    second.client.dispose();
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&socket) }).await);
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&control) }).await);
}

// "passes client plugin packages to a cold server" uses `openClientRuntime` (step 3).
// "retires a cold server after its only Session attachment disconnects" uses `runClient` (step 3); the
// coding agent Session worker it also needs is ported.

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
    let mut listed = Vec::new();
    for route in &routes {
        listed.extend(list_sessions(&route.server_id, &route.path).await);
    }
    let mut expected = pairs(FIRST_ID, &["demo-1", "demo-2"]);
    expected.extend(pairs(SECOND_ID, &["demo-1", "demo-2"]));
    assert_eq!(listed, expected);

    first.close().await.unwrap();
    assert!(wait_until(Duration::from_secs(5), || async { !path_exists(&first.socket_path) }).await);
    let routes =
        pi_client::unix::discover_unix_servers(pi_client::unix::DiscoverUnixServersOptions::new(directory.path()));
    let routes: Vec<String> = routes.await.unwrap().into_iter().map(|route| route.server_id).collect();
    assert_eq!(routes, [SECOND_ID]);
    assert_eq!(list_sessions(SECOND_ID, &second.socket_path).await, pairs(SECOND_ID, &["demo-1", "demo-2"]));
    second.close().await.unwrap();
}

// "reports missing and ambiguous session selections" checks `runClient` Session selection (step 3).

#[tokio::test]
async fn rejects_a_duplicate_session_id_within_one_session_directory() {
    let fixture = fixture().await;
    let other = fixture.agent_dir.path().join("other-cwd").display().to_string();
    let error = create_experimental_sessions(&fixture.sessions, &["demo-1"], &other).await.err().unwrap();
    assert!(error.to_string().contains("Session demo-1 already exists"));
}
