//! `CreateAgentSessionOptions::tool_options` and the per-session `PI_*` variables of bash.

use std::sync::{Arc, Mutex};

use pi_agent::SharedTool;
use pi_coding_agent::agent_session::AgentSession;
use pi_coding_agent::tools::{ShellSpawnContext, ToolsOptions};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::harness::{Options, harness_with};

type Seen = Arc<Mutex<Vec<ShellSpawnContext>>>;

fn recording_options(settings: &pi_coding_agent::settings::SettingsManager, expose: bool) -> (ToolsOptions, Seen) {
    let seen: Seen = Arc::default();
    let sink = seen.clone();
    let mut options = ToolsOptions::from_settings(settings);
    options.bash.expose_session_environment = expose;
    options.bash.spawn_hook = Some(Arc::new(move |context: ShellSpawnContext| {
        sink.lock().unwrap().push(context.clone());
        context
    }));
    (options, seen)
}

async fn run_bash(session: &AgentSession) {
    let bash: SharedTool = session.tool("bash").expect("bash tool");
    let args = json!({ "command": "true" });
    bash.execute("call-1", args, CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
}

#[tokio::test]
async fn bash_sees_the_variables_of_its_own_session() {
    let settings = pi_coding_agent::settings::SettingsManager::in_memory(json!({}));
    let (first_options, first_seen) = recording_options(&settings, true);
    let (second_options, second_seen) = recording_options(&settings, true);
    let first = harness_with(Options::default(), |o| o.tool_options = Some(first_options)).await;
    let second = harness_with(Options::default(), |o| o.tool_options = Some(second_options)).await;
    assert_ne!(first.session.session_id(), second.session.session_id());

    run_bash(&first.session).await;
    run_bash(&second.session).await;
    for (h, seen) in [(&first, &first_seen), (&second, &second_seen)] {
        let env = &seen.lock().unwrap()[0].env;
        assert_eq!(env.get("PI_SESSION_ID"), Some(&h.session.session_id()));
        let file = h.session.session_file().unwrap().display().to_string();
        assert_eq!(env.get("PI_SESSION_FILE"), Some(&file));
        assert_eq!(env.get("PI_PROVIDER").map(String::as_str), Some("local"));
        assert_eq!(env.get("PI_MODEL").map(String::as_str), Some("tiny"));
    }

    // Later changes to the session show up in the next command.
    let other = first.session.model_runtime().get_model("local", "other").unwrap();
    first.session.set_model(other, false).await.unwrap();
    run_bash(&first.session).await;
    assert_eq!(first_seen.lock().unwrap()[1].env.get("PI_MODEL").map(String::as_str), Some("other"));
}

#[tokio::test]
async fn tool_options_can_hide_the_session_variables() {
    let settings = pi_coding_agent::settings::SettingsManager::in_memory(json!({}));
    let (options, seen) = recording_options(&settings, false);
    let h = harness_with(Options::default(), |o| o.tool_options = Some(options)).await;
    run_bash(&h.session).await;
    let env = &seen.lock().unwrap()[0].env;
    assert!(env.keys().all(|name| !name.starts_with("PI_SESSION") && name != "PI_MODEL"), "{env:?}");
}

#[tokio::test]
async fn tool_options_survive_a_tool_reload() {
    let settings = pi_coding_agent::settings::SettingsManager::in_memory(json!({}));
    let (options, seen) = recording_options(&settings, true);
    let h = harness_with(Options::default(), |o| o.tool_options = Some(options)).await;
    h.session.reload();
    run_bash(&h.session).await;
    assert_eq!(seen.lock().unwrap().len(), 1, "the reloaded bash tool keeps the spawn hook");
}
