//! Port of the Session worker cases of upstream `test/experimental-remote-runtime.test.ts` (split from
//! `remote_runtime.rs` to keep files short). Workers run the faux Harness of `faux_worker.rs`.

use std::time::Duration;

use pi_chord::BACKGROUND_CONTEXT;
use pi_coding_agent::experimental::cli::ClientCommand;
use pi_coding_agent::experimental::client::{ClientResult, run_client};
use pi_coding_agent::experimental::process::{kill_pid, process_exists};
use pi_coding_agent::experimental::server_runtime::{StartServerOptions, start_server};
use pi_coding_agent::experimental::services::agent_controller::{
    AGENT_CONTROLLER, AgentController, AgentControllerClient, AgentPromptRequest,
};
use pi_coding_agent::experimental::services::plugins::{
    PRESENTATION_PLUGINS, SESSION_PLUGINS, SessionPlugins, SessionPluginsClient,
};
use pi_coding_agent::experimental::services::sessions::{
    SESSION_DIRECTORY, SESSION_MANAGEMENT, SessionCreateOptions, SessionManagement, SessionManagementClient,
    session_directory_state,
};
use pi_coding_agent::experimental::services::transcript::TRANSCRIPT;

use super::remote_support::{
    attach_client, attach_session, client_options, connect, fixture, list_sessions, temp_dir, wait_until,
};
use super::service_binding::{Errors, create_server_service_binding, create_session_service_binding};

fn session_ids(handle: &pi_chord::services::ServiceHandle) -> Vec<String> {
    let state = session_directory_state(handle).unwrap().unwrap();
    state.sessions.into_iter().map(|session| session.session_id).collect()
}

#[tokio::test]
async fn hydrates_and_mutates_server_session_services_across_framed_clients() {
    let fixture = fixture().await;
    let (_directory, runtime) = fixture.make_server().await;
    let first_client = connect(&runtime.server_id, &runtime.socket_path).await;
    let second_client = connect(&runtime.server_id, &runtime.socket_path).await;
    let errors = Errors::default();
    let services = [&*SESSION_DIRECTORY, &*SESSION_MANAGEMENT];
    let first = create_server_service_binding(&first_client, &services, &errors);
    let second = create_server_service_binding(&second_client, &services, &errors);
    for binding in [&first, &second] {
        assert!(matches!(
            binding.source.connection_state().unwrap(),
            pi_coding_agent::experimental::services::connection::ServerConnectionState::Connected { .. }
        ));
    }
    let catalogue =
        pi_chord::services::RemoteServiceSource::catalogue(first.source.as_ref(), &BACKGROUND_CONTEXT).await.unwrap();
    let ids: Vec<&str> = catalogue.iter().map(|entry| entry.service_id.as_str()).collect();
    assert_eq!(ids, [SESSION_DIRECTORY.id(), SESSION_MANAGEMENT.id(), PRESENTATION_PLUGINS.id()]);
    let (first_directory, second_directory) =
        (first.use_service(&SESSION_DIRECTORY), second.use_service(&SESSION_DIRECTORY));
    let first_management = SessionManagementClient(first.use_service(&SESSION_MANAGEMENT));
    let second_management = SessionManagementClient(second.use_service(&SESSION_MANAGEMENT));
    first.ready(&BACKGROUND_CONTEXT).await.unwrap();
    second.ready(&BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(session_ids(&first_directory), ["demo-1", "demo-2"]);
    assert_eq!(session_directory_state(&second_directory).unwrap(), session_directory_state(&first_directory).unwrap());

    first_management.create(SessionCreateOptions { id: Some("demo-3".into()) }, &BACKGROUND_CONTEXT).await.unwrap();
    let same =
        || session_directory_state(&second_directory).unwrap() == session_directory_state(&first_directory).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || async {
            session_ids(&first_directory).contains(&"demo-3".into()) && same()
        })
        .await
    );

    let (attached_first, attached_second) = tokio::join!(
        first_management.attach("demo-1".into(), &BACKGROUND_CONTEXT),
        second_management.attach("demo-1".into(), &BACKGROUND_CONTEXT),
    );
    attached_first.unwrap();
    attached_second.unwrap();
    assert_eq!(first_client.attachment().map(|target| target.session_id).as_deref(), Some("demo-1"));
    assert_eq!(second_client.attachment().map(|target| target.session_id).as_deref(), Some("demo-1"));
    first_management.remove("demo-1".into(), &BACKGROUND_CONTEXT).await.unwrap();
    let removed = || async {
        first_client.attachment().is_none()
            && second_client.attachment().is_none()
            && !session_ids(&first_directory).contains(&"demo-1".into())
            && same()
    };
    assert!(wait_until(Duration::from_secs(5), removed).await);
    second_management.detach(&BACKGROUND_CONTEXT).await.unwrap();
    assert!(errors.lock().unwrap().is_empty(), "{:?}", errors.lock().unwrap());
    first.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    second.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    runtime.close().await.unwrap();
}

// The Models, model-default, Session plugin, and attachment-fencing cases run the coding agent Session
// worker; they are in `remote_runtime_models.rs`.

// "composes management attachment with Session service hydration" reads the Models service of the coding
// agent Session worker; it is in `remote_runtime_models.rs`.

#[tokio::test]
async fn answers_a_client_prompt_through_the_worker_owned_service_provider() {
    let fixture = fixture().await;
    let (directory, runtime) = fixture.make_server().await;
    let command =
        ClientCommand { session_id: Some("demo-1".into()), prompt: Some("question".into()), ..Default::default() };
    let result = run_client(&command, client_options(directory.path())).await.unwrap();
    assert!(
        matches!(&result, ClientResult::Prompted { text, .. } if text == "deterministic remote answer"),
        "{result:?}"
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn replicates_the_transcript_after_consecutive_prompts() {
    let fixture = fixture().await;
    let (_directory, runtime) = fixture.make_server().await;
    let client = attach_client(&runtime, "demo-1").await;
    let errors = Errors::default();
    let services =
        create_session_service_binding(&client, &[&AGENT_CONTROLLER, &SESSION_PLUGINS, &TRANSCRIPT], &errors);
    let controller = AgentControllerClient(services.use_service(&AGENT_CONTROLLER));
    let transcript = services.use_service(&TRANSCRIPT);
    let plugins = SessionPluginsClient(services.use_service(&SESSION_PLUGINS));
    services.ready(&BACKGROUND_CONTEXT).await.unwrap();
    plugins.reload(&BACKGROUND_CONTEXT).await.unwrap();
    for (count, message) in [(1, "first question"), (2, "second question")] {
        let response = controller.prompt(AgentPromptRequest::text(message), &BACKGROUND_CONTEXT).await.unwrap();
        assert!(response.accepted, "{response:?}");
        let settled = controller.wait_for_prompt(response.operation_id.unwrap(), &BACKGROUND_CONTEXT).await.unwrap();
        assert_eq!((settled.status.as_str(), settled.text.as_deref()), ("done", Some("deterministic remote answer")));
        let assistants = || {
            let value = transcript.state("state").unwrap().value().unwrap().map(|value| value.to_serde());
            let entries = value.and_then(|value| value["entries"].as_array().cloned()).unwrap_or_default();
            entries.iter().filter(|entry| entry["kind"] == "pi.assistant").count()
        };
        assert!(wait_until(Duration::from_secs(5), || async { assistants() == count }).await);
    }
    services.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    client.dispose();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn stops_an_idle_session_worker_after_its_client_disconnects() {
    let fixture = fixture().await;
    let (_directory, runtime) = fixture.make_server().await;
    let client = attach_client(&runtime, "demo-1").await;
    let pid = runtime.worker_pids()["demo-1"];
    client.dispose();
    assert!(wait_until(Duration::from_secs(10), || async { !runtime.worker_pids().contains_key("demo-1") }).await);
    assert!(wait_until(Duration::from_secs(5), || async { !process_exists(pid) }).await);
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn starts_one_process_per_attached_session_and_stops_them_during_shutdown() {
    let fixture = fixture().await;
    let (_directory, runtime) = fixture.make_server().await;
    let (_first, _second) = tokio::join!(attach_client(&runtime, "demo-1"), attach_client(&runtime, "demo-2"));
    let pids: Vec<u32> = runtime.worker_pids().into_values().collect();
    assert_eq!(pids.len(), 2);
    assert_ne!(pids[0], pids[1]);
    assert!(pids.iter().all(|pid| process_exists(*pid)));
    runtime.close().await.unwrap();
    assert!(runtime.worker_pids().is_empty());
    for pid in pids {
        assert!(wait_until(Duration::from_secs(5), || async { !process_exists(pid) }).await);
    }
}

#[tokio::test]
async fn server_runtime_replaces_an_exited_worker_on_the_next_attach() {
    let fixture = fixture().await;
    let (_directory, runtime) = fixture.make_server().await;
    let client = attach_client(&runtime, "demo-1").await;
    let first_pid = runtime.worker_pids()["demo-1"];
    kill_pid(first_pid);
    assert!(wait_until(Duration::from_secs(5), || async { !runtime.worker_pids().contains_key("demo-1") }).await);
    attach_session(&client, "demo-1").await.unwrap();
    let replacement = runtime.worker_pids()["demo-1"];
    assert_ne!(replacement, first_pid);
    client.dispose();
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn discovers_workers_after_replacing_the_server() {
    let fixture = fixture().await;
    let directory = temp_dir("per-");
    let first = start_server(fixture.options(directory.path())).await.unwrap();
    let _first_client = attach_client(&first, "demo-1").await;
    let worker_pid = first.worker_pids()["demo-1"];
    let replacement = start_server(fixture.options(directory.path())).await.unwrap();
    first.closed().await.unwrap();
    assert_eq!(replacement.server_id, first.server_id);
    assert_eq!(replacement.worker_pids().get("demo-1"), Some(&worker_pid));
    assert!(wait_until(Duration::from_secs(5), || async { first.worker_pids().is_empty() }).await);
    assert!(process_exists(worker_pid));
    let expected: Vec<(String, String)> = ["demo-1", "demo-2"].map(|id| (first.server_id.clone(), id.into())).into();
    assert_eq!(list_sessions(directory.path()).await, expected);
    let _again = attach_client(&replacement, "demo-1").await;
    assert_eq!(replacement.worker_pids().get("demo-1"), Some(&worker_pid));
    let _second = attach_client(&replacement, "demo-2").await;
    assert_ne!(replacement.worker_pids().get("demo-2"), Some(&worker_pid));
    replacement.close().await.unwrap();
}

// "retires an unclaimed idle worker after replacement demand expires" sets the worker's orphan grace
// through the environment, which Rust tests cannot change without unsafe code.

#[tokio::test]
async fn restores_tracked_sessions_that_are_outside_the_replacement_catalog() {
    let fixture = fixture().await;
    let (directory, empty_sessions) = (temp_dir("pet-"), temp_dir("pet-sessions-"));
    let first = start_server(fixture.options(directory.path())).await.unwrap();
    let _client = attach_client(&first, "demo-1").await;
    let worker_pid = first.worker_pids()["demo-1"];
    let options = StartServerOptions {
        session_dir: Some(empty_sessions.path().display().to_string()),
        ..fixture.options(directory.path())
    };
    let replacement = start_server(options).await.unwrap();
    first.closed().await.unwrap();
    let listed = list_sessions(directory.path()).await;
    assert_eq!(listed, [(first.server_id.clone(), "demo-1".to_string())]);
    let _again = attach_client(&replacement, "demo-1").await;
    assert_eq!(replacement.worker_pids().get("demo-1"), Some(&worker_pid));
    replacement.close().await.unwrap();
}

#[tokio::test]
async fn observes_keyed_service_instances_and_fences_replacement_generations_over_framed_transport() {
    use pi_chord::facets::{FacetOptions, create_facet_host, define_facet};
    use pi_chord::services::{RemoteServiceSource, ServiceHandle, ServiceMode, observe_handler};
    use pi_coding_agent::experimental::services::connection::{SessionAttachmentState, SessionServiceSource};
    use std::sync::{Arc, Mutex};

    let fixture = fixture().await;
    let (_directory, runtime) = fixture.make_server().await;
    let client = attach_client(&runtime, "demo-1").await;
    let errors = Errors::default();
    let error_log = errors.clone();
    let source = SessionServiceSource::new(
        &client,
        Some(Arc::new(move |error| error_log.lock().unwrap().push(error.to_string()))),
    )
    .unwrap();
    let catalogue = source.catalogue(&BACKGROUND_CONTEXT).await.unwrap();
    assert!(
        catalogue
            .iter()
            .any(|entry| entry.service_id == super::faux_worker::KEYED_PROBE.id() && entry.mode == ServiceMode::Keyed)
    );
    let observed = Arc::new(Mutex::new(Vec::<(ServiceHandle, Option<String>)>::new()));
    let record = observed.clone();
    let consumer = define_facet("@test/keyed-probe-consumer", move |env| {
        let record = record.clone();
        let handler = observe_handler(move |service: ServiceHandle, _context| {
            let value = service.state("state").ok().and_then(|state| state.value().ok().flatten());
            let value = value.and_then(|value| value.get("value").and_then(|value| value.as_str().map(str::to_string)));
            record.lock().unwrap().push((service, value));
            async { Ok(()) }
        });
        env.observe(&super::faux_worker::KEYED_PROBE, handler)
    });
    let options = FacetOptions { service_sources: vec![source.clone()], ..FacetOptions::new(vec![consumer]) };
    let host = create_facet_host(options).await.unwrap();
    let attached = || Some(SessionAttachmentState::Attached { session_id: "demo-1".into() });
    assert_eq!(source.attachment_state(), attached());
    let count = || observed.lock().unwrap().len();
    assert!(wait_until(Duration::from_secs(5), || async { count() == 1 }).await);
    let first = observed.lock().unwrap()[0].clone();
    assert_eq!(first.1.as_deref(), Some("first"));
    first.0.call("replace", vec![pi_chord::Json::from("second")], &BACKGROUND_CONTEXT).await.unwrap();
    assert!(wait_until(Duration::from_secs(5), || async { count() == 2 }).await);
    let second = observed.lock().unwrap()[1].clone();
    assert_eq!(second.1.as_deref(), Some("second"));
    let stale = first.0.call("replace", vec![pi_chord::Json::from("late")], &BACKGROUND_CONTEXT).await.unwrap_err();
    assert!(stale.to_string().contains("observation is closed"), "{stale}");

    attach_session(&client, "demo-2").await.unwrap();
    source.when_attached("demo-2", &BACKGROUND_CONTEXT).await.unwrap();
    assert!(wait_until(Duration::from_secs(5), || async { count() == 3 }).await);
    assert_eq!(source.attachment_state(), Some(SessionAttachmentState::Attached { session_id: "demo-2".into() }));
    assert_eq!(observed.lock().unwrap()[2].1.as_deref(), Some("first"));
    let replaced = second.0.call("replace", vec![pi_chord::Json::from("late")], &BACKGROUND_CONTEXT).await.unwrap_err();
    assert!(replaced.to_string().contains("observation is closed"), "{replaced}");
    assert!(errors.lock().unwrap().is_empty(), "{:?}", errors.lock().unwrap());
    host.dispose().await.unwrap();
    source.dispose(&BACKGROUND_CONTEXT).await.unwrap();
    client.dispose();
    runtime.close().await.unwrap();
}
