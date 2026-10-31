//! pi as an ACP client end to end: a session whose model is the fake ACP agent of pi-ai, which
//! this test binary runs when started with [`AGENT_ENV`].

#[path = "../../pi-ai/tests/support/fake_acp_agent.rs"]
mod fake_agent;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use std::sync::Arc;

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::{ContentBlock, Message};
use pi_coding_agent::acp_client::host;
use pi_coding_agent::agent_session::{
    AgentSession, CreateAgentSessionOptions, CreateServicesOptions, ResourceOptions, UndoSkip, create_agent_session,
    create_agent_session_services,
};
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use serde_json::json;

const AGENT_ENV: &str = "PI_TEST_FAKE_ACP_AGENT";
const TEST_NAME: &str = "fake_acp_agent_process";

/// The fake agent, when this binary runs as one; a no-op test otherwise.
#[test]
fn fake_acp_agent_process() {
    if std::env::var_os(AGENT_ENV).is_some() {
        fake_agent::main();
    }
}

fn last_reply(session: &AgentSession) -> String {
    let messages = session.messages();
    let Some(Message::Assistant(reply)) = messages.iter().rev().find(|m| matches!(m, Message::Assistant(_))) else {
        panic!("no reply in {messages:?}");
    };
    assert!(reply.error_message.is_none(), "{:?}", reply.error_message);
    reply.content.iter().filter_map(ContentBlock::as_text).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_turns_undo_agent_edits_and_survive_compaction() {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    for (name, text) in [("b.txt", "old b\n"), ("same.txt", "ABC\n"), ("gone.txt", "bye\n"), ("keep.txt", "mine\n")] {
        std::fs::write(cwd.join(name), text).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let settings = Arc::new(SettingsManager::in_memory(json!({
        "defaultProvider": "fake-acp", "defaultModel": "default",
        "compaction": {"keepRecentTokens": 1},
        "acpAgents": [{"id": "fake", "name": "Fake agent", "command": exe,
            "args": ["--exact", TEST_NAME, "--nocapture", "--quiet", "--test-threads=1"], "env": {AGENT_ENV: "1"}}]})));
    let services = create_agent_session_services(CreateServicesOptions {
        settings: Some(settings),
        credentials: Some(Arc::new(InMemoryCredentialStore::default())),
        credential_env: Some(offline_guard::credential_free_env()),
        resources: ResourceOptions { home: Some(root.path().to_path_buf()), ..ResourceOptions::default() },
        ..CreateServicesOptions::new(&cwd, &agent_dir)
    })
    .await;
    let manager = SessionManager::in_memory(&cwd);
    let session = create_agent_session(CreateAgentSessionOptions::new(services, manager)).await.unwrap().session;
    assert_eq!(session.model().map(|model| model.provider).as_deref(), Some("fake-acp"));

    session.prompt("own-edit", Vec::new()).await.unwrap();
    assert!(last_reply(&session).contains("turn=1"), "{}", last_reply(&session));
    let window = session.model_runtime().get_model("fake-acp", "default").map(|model| model.context_window);
    assert_eq!(window, Some(200_000), "the size of the agent's usage_update");
    let commands: Vec<String> = host::commands(&agent_dir, "fake-acp").into_iter().map(|c| c.name).collect();
    assert_eq!(commands, ["web", "compact"]);

    let work = cwd.canonicalize().unwrap();
    let mut candidates: Vec<(String, bool)> = session
        .undo_candidates()
        .into_iter()
        .map(|c| (c.path.strip_prefix(&work).unwrap().display().to_string(), c.detected))
        .collect();
    candidates.sort();
    let expected = [("b.txt", false), ("c.txt", true), ("gone.txt", true), ("same.txt", true)];
    assert_eq!(candidates, expected.map(|(name, detected)| (name.to_string(), detected)));
    std::fs::write(cwd.join("c.txt"), "user change\n").unwrap();
    let report = session.undo_file_changes_with(true).unwrap();
    assert_eq!(report.skipped, vec![(work.join("c.txt"), UndoSkip::ChangedSince)]);
    for (name, text) in [("b.txt", "old b\n"), ("same.txt", "ABC\n"), ("gone.txt", "bye\n"), ("c.txt", "user change\n")]
    {
        assert_eq!(std::fs::read_to_string(cwd.join(name)).unwrap(), text, "{name}");
    }

    session.prompt("/acp:compact now", Vec::new()).await.unwrap();
    assert!(last_reply(&session).contains("command=/compact now"), "{}", last_reply(&session));
    session.prompt("hello", Vec::new()).await.unwrap();
    let compaction = session.compact(None).await.unwrap();
    assert!(compaction.summary.contains("turn=1"), "the summary ran in a new session: {}", compaction.summary);
    session.prompt("again", Vec::new()).await.unwrap();
    let reply = last_reply(&session);
    assert!(reply.starts_with("Hello [text] mode=ask model=fast turn=4 "), "the agent session continued: {reply}");
    let state = host::read_state(&agent_dir);
    assert_eq!(state.sessions.len(), 1, "the summary session is not recorded: {:?}", state.sessions);
}
