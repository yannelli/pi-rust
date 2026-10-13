//! Session replacement through the runtime (upstream `agent-session-runtime` and
//! `agent-session-branching` tests).

use std::sync::{Arc, Mutex};

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{AgentSessionRuntime, ForkPosition};
use pi_coding_agent::app::create_app_runtime;
use pi_coding_agent::session::SessionManager;

use crate::harness::{Harness, Options, harness, roles, text};

async fn runtime_with_turns(persist: bool) -> (Harness, AgentSessionRuntime) {
    let responses =
        vec![FauxResponse::text("first answer"), FauxResponse::text("second answer"), FauxResponse::text("third")];
    let h = harness(Options { responses, persist, ..Options::default() }).await;
    let manager = if persist {
        SessionManager::create(&h.cwd, Some(&h.agent_dir.join("sessions")))
    } else {
        SessionManager::in_memory(&h.cwd)
    };
    let runtime = create_app_runtime(h.app_options(), manager).await.unwrap();
    let session = runtime.session();
    session.prompt("first question", Vec::new()).await.unwrap();
    session.prompt("second question", Vec::new()).await.unwrap();
    (h, runtime)
}

fn user_ids(runtime: &AgentSessionRuntime) -> Vec<String> {
    runtime.session().user_messages_for_forking().into_iter().map(|(id, _)| id).collect()
}

#[tokio::test]
async fn starts_new_sessions_and_switches_back_with_rebinding() {
    let (_h, runtime) = runtime_with_turns(true).await;
    let first = runtime.session();
    let first_file = first.session_file().unwrap();
    let rebound = Arc::new(Mutex::new(Vec::new()));
    let sink = rebound.clone();
    runtime.set_rebind_session(Some(Arc::new(move |session| sink.lock().unwrap().push(session.session_id()))));

    runtime.new_session(None, None).await.unwrap();
    let fresh = runtime.session();
    assert_ne!(fresh.session_id(), first.session_id());
    assert!(fresh.messages().is_empty());
    assert_eq!(fresh.model().unwrap().id, "tiny");

    runtime.switch_session(&first_file, None).await.unwrap();
    let resumed = runtime.session();
    assert_eq!(resumed.session_id(), first.session_id());
    assert_eq!(roles(&resumed.messages()), ["system", "user", "assistant", "user", "assistant"]);
    assert_eq!(rebound.lock().unwrap().clone(), [fresh.session_id(), first.session_id()]);
    runtime.dispose().await;
}

#[tokio::test]
async fn forks_before_a_user_message_and_clones_the_branch() {
    let (_h, runtime) = runtime_with_turns(true).await;
    let original = runtime.session().session_file().unwrap();
    let second = user_ids(&runtime)[1].clone();

    let forked = runtime.fork(&second, ForkPosition::Before).await.unwrap();
    assert_eq!(forked.selected_text.as_deref(), Some("second question"));
    let session = runtime.session();
    assert_ne!(session.session_file().unwrap(), original);
    assert_eq!(roles(&session.messages()), ["system", "user", "assistant"]);
    let header = session.session_manager().header_raw().clone();
    assert_eq!(header["parentSession"].as_str().unwrap(), original.display().to_string());

    let cloned = runtime.clone_session().await.unwrap();
    assert_eq!(cloned.selected_text, None);
    assert_eq!(roles(&runtime.session().messages()), ["system", "user", "assistant"]);
    let error = runtime.fork("missing", ForkPosition::At).await.unwrap_err();
    assert_eq!(error.to_string(), "Invalid entry ID for forking");
}

#[tokio::test]
async fn forks_in_memory_sessions() {
    let (_h, runtime) = runtime_with_turns(false).await;
    let first = user_ids(&runtime)[0].clone();
    let result = runtime.fork(&first, ForkPosition::Before).await.unwrap();
    assert_eq!(result.selected_text.as_deref(), Some("first question"));
    assert!(runtime.session().session_file().is_none());
    assert!(runtime.session().messages().iter().all(|message| message.role() == "system"));
    runtime.session().prompt("again", Vec::new()).await.unwrap();
    assert_eq!(text(runtime.session().messages().last().unwrap()), "third");
}

#[tokio::test]
async fn reports_unsaved_sessions_and_imports_jsonl_files() {
    let (h, runtime) = runtime_with_turns(true).await;
    let source = runtime.session().session_file().unwrap();
    let copy = h.cwd.join("exported.jsonl");
    std::fs::copy(&source, &copy).unwrap();

    runtime.new_session(None, None).await.unwrap();
    let leaf = runtime.session().session_manager().leaf_id().unwrap().to_string();
    let error = runtime.fork(&leaf, ForkPosition::At).await.unwrap_err();
    assert_eq!(error.to_string(), "This session has not been saved yet. Send a message before cloning or forking it.");

    let missing = runtime.import_from_jsonl(&h.cwd.join("nope.jsonl"), None).await.unwrap_err();
    assert!(missing.to_string().starts_with("File not found: "));
    runtime.import_from_jsonl(&copy, None).await.unwrap();
    let imported = runtime.session();
    assert_eq!(roles(&imported.messages()), ["system", "user", "assistant", "user", "assistant"]);
    let file = imported.session_file().unwrap();
    assert!(file.starts_with(h.agent_dir.join("sessions")));
    assert_eq!(imported.cwd(), h.cwd.as_path());
}
