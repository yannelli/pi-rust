//! The cases of upstream `test/experimental-remote-runtime.test.ts` that need the coding agent Session worker:
//! its model runtime, settings, and plugin loading. Their agent directories carry `REAL_AGENT_PREFIX`, so
//! `faux_worker.rs` runs the real worker for them.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::BACKGROUND_CONTEXT;
use pi_chord::services::ServiceRef;
use pi_chord::state::StateListener;
use pi_coding_agent::experimental::server_runtime::{RunningServer, StartServerOptions, start_server};
use pi_coding_agent::experimental::services::connection::SessionAttachmentState;
use pi_coding_agent::experimental::services::models::{MODELS, ModelRef, Models, ModelsClient};
use pi_coding_agent::experimental::services::plugins::{
    PRESENTATION_PLUGINS, PrepareSessionRequest, PresentationPlugins, PresentationPluginsClient,
};
use pi_coding_agent::experimental::services::sessions::{
    SESSION_MANAGEMENT, SessionManagement, SessionManagementClient,
};
use pi_coding_agent::experimental::session_catalog::{read_session, session_storage_path};
use pi_durable::harness::AGENT_DOC;
use pi_durable::session::{SessionOptions, create_session};
use pi_durable::storage::open_sqlite_storage;
use pi_durable::{ROOT_CONVERSATION_ID, Storage};
use serde_json::json;

use super::faux_worker::{self, REAL_AGENT_PREFIX};
use super::remote_support::{
    attach_client, attach_session, connect, create_experimental_sessions, temp_dir, wait_until,
};
use super::service_binding::{Errors, create_server_service_binding, create_session_service_binding};

fn sonnet() -> Option<ModelRef> {
    Some(ModelRef { provider: "anthropic".into(), model_id: "claude-sonnet-4-5".into() })
}

fn opus() -> Option<ModelRef> {
    Some(ModelRef { provider: "anthropic".into(), model_id: "claude-opus-4-6".into() })
}

/// An agent directory with Anthropic credentials (upstream `configureExperimentalWorkerModel`) and the
/// Sessions `demo-1` and `demo-2`.
struct RealFixture {
    agent_dir: tempfile::TempDir,
}

async fn real_fixture() -> RealFixture {
    faux_worker::install();
    let agent_dir = tempfile::Builder::new().prefix(REAL_AGENT_PREFIX).tempdir_in("/tmp").unwrap();
    let auth = json!({ "anthropic": { "type": "api_key", "key": "test-key" } });
    std::fs::write(agent_dir.path().join("auth.json"), auth.to_string()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(agent_dir.path().join("auth.json"), std::fs::Permissions::from_mode(0o600)).unwrap();
    let fixture = RealFixture { agent_dir };
    let cwd = std::env::current_dir().unwrap().display().to_string();
    create_experimental_sessions(&fixture.sessions(), &["demo-1", "demo-2"], &cwd).await.unwrap();
    fixture
}

impl RealFixture {
    fn sessions(&self) -> std::path::PathBuf {
        self.agent_dir.path().join("experimental").join("sessions")
    }

    fn options(&self, directory: &Path, model: bool) -> StartServerOptions {
        StartServerOptions {
            directory: Some(directory.display().to_string()),
            session_dir: Some(self.sessions().display().to_string()),
            provider: model.then(|| "anthropic".into()),
            model: model.then(|| "claude-sonnet-4-5".into()),
            ..StartServerOptions::default()
        }
    }
}

async fn wait_for_worker_exit(runtime: &RunningServer, session_id: &str) {
    assert!(wait_until(Duration::from_secs(10), || async { !runtime.worker_pids().contains_key(session_id) }).await);
}

/// Upstream `readExperimentalSessionState`: the root conversation's model, read while no worker owns it.
async fn read_session_model(sessions: &Path, session_id: &str) -> Option<ModelRef> {
    let metadata = read_session(sessions, session_id).await.expect("Session exists");
    let storage = open_sqlite_storage(&session_storage_path(&metadata).display().to_string()).await.unwrap();
    let storage: Arc<dyn Storage> = Arc::new(storage);
    let session = create_session(storage, SessionOptions::default());
    let agent = session.snapshot(&AGENT_DOC, ROOT_CONVERSATION_ID, &BACKGROUND_CONTEXT).await.unwrap();
    session.close(&BACKGROUND_CONTEXT).await.unwrap();
    serde_json::from_value(agent?.to_serde()["model"].clone()).ok()
}

fn models_state(models: &ModelsClient) -> Option<pi_coding_agent::experimental::services::models::ModelsState> {
    models.state().unwrap()
}

#[tokio::test]
async fn preserves_an_existing_session_model_when_the_server_default_changes() {
    let fixture = real_fixture().await;
    let settings = json!({ "defaultProvider": "anthropic", "defaultModel": "claude-opus-4-6" });
    std::fs::write(fixture.agent_dir.path().join("settings.json"), settings.to_string()).unwrap();
    let directory = temp_dir("pes-");
    let first = start_server(fixture.options(directory.path(), false)).await.unwrap();
    let first_client = attach_client(&first, "demo-1").await;
    first_client.dispose();
    wait_for_worker_exit(&first, "demo-1").await;
    first.close().await.unwrap();

    let second = start_server(fixture.options(directory.path(), true)).await.unwrap();
    let second_client = attach_client(&second, "demo-1").await;
    second_client.dispose();
    wait_for_worker_exit(&second, "demo-1").await;
    assert_eq!(read_session_model(&second.session_dir, "demo-1").await, opus());
    second.close().await.unwrap();
}

#[tokio::test]
async fn hydrates_and_updates_the_models_service_across_concurrent_framed_clients() {
    let fixture = real_fixture().await;
    let directory = temp_dir("pes-");
    let runtime = start_server(fixture.options(directory.path(), true)).await.unwrap();
    let first_client = attach_client(&runtime, "demo-1").await;
    let worker_pid = runtime.worker_pids()["demo-1"];
    let second_client = attach_client(&runtime, "demo-1").await;
    assert_eq!(runtime.worker_pids().get("demo-1"), Some(&worker_pid));
    let errors = Errors::default();
    let first = create_session_service_binding(&first_client, &[&MODELS], &errors);
    let second = create_session_service_binding(&second_client, &[&MODELS], &errors);
    let (first_models, second_models) =
        (ModelsClient(first.use_service(&MODELS)), ModelsClient(second.use_service(&MODELS)));
    let (ready_first, ready_second) = tokio::join!(first.ready(&BACKGROUND_CONTEXT), second.ready(&BACKGROUND_CONTEXT));
    ready_first.unwrap();
    ready_second.unwrap();
    let attached = Some(SessionAttachmentState::Attached { session_id: "demo-1".into() });
    assert_eq!(first.source.attachment_state(), attached);
    assert_eq!(second.source.attachment_state(), attached);
    let state = models_state(&first_models).unwrap();
    assert_eq!(state.configuration.model, sonnet());
    assert_eq!(models_state(&second_models), Some(state.clone()));
    let previous = state.configuration.thinking_level;
    first_models.cycle_thinking(&BACKGROUND_CONTEXT).await.unwrap();
    let changed = || async {
        let first_state = models_state(&first_models);
        first_state.as_ref().is_some_and(|state| state.configuration.thinking_level != previous)
            && models_state(&second_models) == first_state
    };
    assert!(wait_until(Duration::from_secs(5), changed).await);
    assert!(errors.lock().unwrap().is_empty(), "{:?}", errors.lock().unwrap());
    first.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    second.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn loads_conventional_session_facets_from_multiple_configured_plugin_packages() {
    let fixture = real_fixture().await;
    let directory = temp_dir("pes-plugin-");
    let second_package = directory.path().join("second-plugin");
    std::fs::create_dir_all(second_package.join("src")).unwrap();
    let manifest = json!({
        "name": "@earendil-works/second-session-plugin",
        "version": "1.0.0",
        "peerDependencies": { "@earendil-works/chord": "^0.84.4" },
    });
    std::fs::write(second_package.join("package.json"), format!("{manifest}\n")).unwrap();
    std::fs::write(
        second_package.join("src").join("session.ts"),
        "import { defineFacet, defineService } from \"@earendil-works/chord\"; const Service = defineService(\"test.second-plugin\"); export default defineFacet({ id: \"second-session-plugin\", setup(env) { env.provide(Service, { async read() { return \"second\"; } }); } });\n",
    )
    .unwrap();
    let runtime = start_server(fixture.options(directory.path(), true)).await.unwrap();
    // Upstream goes through `openClientRuntime` and `activateBuiltinClientServices` (step 3); the server
    // services they wrap are used directly here.
    let client = connect(&runtime.server_id, &runtime.socket_path).await;
    let errors = Errors::default();
    let server = create_server_service_binding(&client, &[&PRESENTATION_PLUGINS, &SESSION_MANAGEMENT], &errors);
    let plugins = PresentationPluginsClient(server.use_service(&PRESENTATION_PLUGINS));
    let management = SessionManagementClient(server.use_service(&SESSION_MANAGEMENT));
    server.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let example = env!("CARGO_MANIFEST_DIR").to_string() + "/examples/plugins/pi-example-plugin";
    let package_paths = Some(vec![example, second_package.display().to_string()]);
    let request = PrepareSessionRequest { session_id: "demo-1".into(), package_paths };
    plugins.prepare_session(request, &BACKGROUND_CONTEXT).await.unwrap();
    management.attach("demo-1".into(), &BACKGROUND_CONTEXT).await.unwrap();
    let (example_service, second_service) =
        (ServiceRef::remote("pi.example-plugin.greeting"), ServiceRef::remote("test.second-plugin"));
    let services = create_session_service_binding(&client, &[&example_service, &second_service], &errors);
    services.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let greeting = services.use_service(&example_service);
    let reply = greeting.call("greet", vec![json!({ "name": "Armin" }).into()], &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(reply.map(|reply| reply.to_serde()["workerActivations"].clone()), Some(json!(1)));
    let second = services.use_service(&second_service).call("read", Vec::new(), &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(second.map(|value| value.to_serde()), Some(json!("second")));
    services.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    server.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    client.dispose();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn uses_the_most_recently_selected_model_for_a_new_session() {
    let fixture = real_fixture().await;
    let directory = temp_dir("pes-model-default-");
    let runtime = start_server(fixture.options(directory.path(), false)).await.unwrap();
    let first_client = attach_client(&runtime, "demo-1").await;
    let errors = Errors::default();
    let first = create_session_service_binding(&first_client, &[&MODELS], &errors);
    let first_models = ModelsClient(first.use_service(&MODELS));
    first.ready(&BACKGROUND_CONTEXT).await.unwrap();
    first_models.select(opus().unwrap(), &BACKGROUND_CONTEXT).await.unwrap();
    first.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    first_client.dispose();
    wait_for_worker_exit(&runtime, "demo-1").await;

    let second_client = attach_client(&runtime, "demo-2").await;
    let second = create_session_service_binding(&second_client, &[&MODELS], &errors);
    let second_models = ModelsClient(second.use_service(&MODELS));
    second.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(models_state(&second_models).unwrap().configuration.model, opus());
    second.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    second_client.dispose();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn fences_superseded_attachment_hydration_by_attachment_generation() {
    let fixture = real_fixture().await;
    let directory = temp_dir("pes-");
    let runtime = start_server(fixture.options(directory.path(), true)).await.unwrap();
    let client = connect(&runtime.server_id, &runtime.socket_path).await;
    let errors = Errors::default();
    let services = create_session_service_binding(&client, &[&MODELS], &errors);
    let models = ModelsClient(services.use_service(&MODELS));
    services.ready(&BACKGROUND_CONTEXT).await.unwrap();
    let states: Arc<Mutex<Vec<SessionAttachmentState>>> = Arc::default();
    let recorded = states.clone();
    let listener = services.source.attachment().subscribe(StateListener::sync(move |value, _, _| {
        if let Ok(state) = serde_json::from_value(value.to_serde()) {
            recorded.lock().unwrap().push(state);
        }
        Ok(())
    }));
    // Upstream delays the `demo-2` Models subscription with a spy on `Client.subscribeService`, and checks
    // that the binding is still attaching `demo-2`. A Rust `Client` cannot be spied on; the second attach
    // still supersedes the first one's hydration.
    attach_session(&client, "demo-2").await.unwrap();
    attach_session(&client, "demo-1").await.unwrap();
    services.source.when_attached("demo-1", &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(
        services.source.attachment_state(),
        Some(SessionAttachmentState::Attached { session_id: "demo-1".into() })
    );
    assert_eq!(models_state(&models).unwrap().configuration.model, sonnet());
    listener.unsubscribe();
    services.dispose(&BACKGROUND_CONTEXT).await.unwrap();

    let states = states.lock().unwrap().clone();
    let attaching = SessionAttachmentState::Attaching { session_id: "demo-1".into() };
    let latest = states.iter().rposition(|state| *state == attaching).expect("attaching demo-1");
    let after = &states[latest..];
    assert!(!after.contains(&SessionAttachmentState::Attached { session_id: "demo-2".into() }));
    assert!(!after.contains(&SessionAttachmentState::Degraded { session_id: "demo-2".into() }));
    assert!(errors.lock().unwrap().is_empty(), "{:?}", errors.lock().unwrap());
    client.dispose();
    runtime.close().await.unwrap();
}
