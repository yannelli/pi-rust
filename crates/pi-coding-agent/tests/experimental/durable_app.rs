//! Checks of the `durable/` and `vacation/` runtimes. Upstream has no tests for them; these open each app
//! in a temporary agent directory and drive the controller.

use std::time::Duration;

use pi_coding_agent::experimental::durable::runtime::NoticeLevel;
use pi_coding_agent::experimental::durable::{
    DurableApp, OpenDurableOptions, agent_of, open_durable, parse_durable_args,
};

fn options(agent_dir: &std::path::Path, cwd: &std::path::Path, continue_session: bool) -> OpenDurableOptions {
    OpenDurableOptions { cwd: Some(cwd.to_path_buf()), continue_session, agent_dir: Some(agent_dir.to_path_buf()) }
}

#[test]
fn parses_the_continue_flag_and_rejects_other_arguments() {
    assert!(parse_durable_args(&["--continue".into()]).unwrap().continue_session);
    assert!(parse_durable_args(&["-c".into()]).unwrap().continue_session);
    assert!(!parse_durable_args(&[]).unwrap().continue_session);
    assert_eq!(parse_durable_args(&["--x".into()]).err().unwrap().to_string(), "Unknown argument: --x");
}

#[tokio::test]
async fn opens_continues_and_locks_durable_sessions() {
    let (agent_dir, cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let missing = open_durable(DurableApp::Coding, options(agent_dir.path(), cwd.path(), true)).await;
    assert!(missing.err().unwrap().to_string().starts_with("No durable session exists for "));

    let opened = open_durable(DurableApp::Coding, options(agent_dir.path(), cwd.path(), false)).await.unwrap();
    let view = opened.view.current();
    assert_eq!(view.session.cwd, std::fs::canonicalize(cwd.path()).unwrap().display().to_string());
    assert!(view.session.directory.starts_with(agent_dir.path().join("experimental").join("durable-sessions")));
    assert_eq!(view.conversations.iter().map(|summary| summary.label.as_str()).collect::<Vec<_>>(), ["main"]);
    // The process environment may hold provider keys, and then pi's default resolution picks a model.
    let unconfigured = agent_of(&view.conversation).model.is_none();
    let warned = view.notices.iter().any(|notice| {
        notice.level == NoticeLevel::Warning && notice.message == "No model configured; select one with /model."
    });
    assert_eq!(warned, unconfigured);
    assert!(view.tasks.is_some(), "the task panel starts open");
    opened.controller.toggle_tasks().await;
    assert!(opened.view.current().tasks.is_none());
    if unconfigured {
        opened.controller.cycle_thinking().await;
        assert_eq!(opened.view.current().notices.last().unwrap().message, "No model selected");
    }

    let second = open_durable(DurableApp::Coding, options(agent_dir.path(), cwd.path(), true));
    let blocked = tokio::time::timeout(Duration::from_millis(500), second).await;
    assert!(blocked.is_err(), "a second process waits for the lock");
    let id = view.session.id.clone();
    opened.close().await.unwrap();

    let continued = open_durable(DurableApp::Coding, options(agent_dir.path(), cwd.path(), true)).await.unwrap();
    assert_eq!(continued.view.current().session.id, id);
    continued.close().await.unwrap();
}

#[tokio::test]
async fn opens_the_vacation_planner_with_its_own_sessions() {
    let (agent_dir, cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let opened = open_durable(DurableApp::Vacation, options(agent_dir.path(), cwd.path(), false)).await.unwrap();
    let view = opened.view.current();
    assert!(view.session.directory.starts_with(agent_dir.path().join("experimental").join("vacation-sessions")));
    let extensions = &view.conversation["docs"]["pi.agent"]["extensions"];
    assert_eq!(extensions, &serde_json::json!(["vacation"]));
    opened.close().await.unwrap();
}

#[tokio::test]
async fn opens_a_long_session_with_the_default_window_of_its_newest_fifty_entries() {
    use pi_chord::BACKGROUND_CONTEXT;
    use pi_durable::session::{SessionOptions, Tx, create_session};
    use pi_durable::{EntryDraft, ROOT_CONVERSATION_ID};

    let (agent_dir, cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let opened = open_durable(DurableApp::Coding, options(agent_dir.path(), cwd.path(), false)).await.unwrap();
    let database = std::path::Path::new(&opened.view.current().session.directory).join("session.sqlite");
    let before = opened.view.current().conversation["entries"].as_array().unwrap().len();
    opened.close().await.unwrap();

    let storage = pi_durable::storage::open_sqlite_storage(&database.display().to_string()).await.unwrap();
    let session = create_session(std::sync::Arc::new(storage), SessionOptions::default());
    let append = |tx: Tx| async move {
        let mut ids = Vec::new();
        for _ in 0..60 {
            ids.push(tx.append_entry(ROOT_CONVERSATION_ID, EntryDraft::new("app.note")).await?.id);
        }
        Ok(ids)
    };
    let written = session.commit(append, &BACKGROUND_CONTEXT).await.unwrap();
    session.close(&BACKGROUND_CONTEXT).await.unwrap();

    let continued = open_durable(DurableApp::Coding, options(agent_dir.path(), cwd.path(), true)).await.unwrap();
    let view = continued.view.current().conversation;
    let ids: Vec<u64> = view["entries"].as_array().unwrap().iter().map(|entry| entry["id"].as_u64().unwrap()).collect();
    let newest: Vec<u64> = written[10..].iter().map(|id| id.get()).collect();
    assert_eq!(ids, newest, "{before} entries before the notes");
    assert_eq!(view["before"], serde_json::json!(written[9]));
    continued.close().await.unwrap();
}
