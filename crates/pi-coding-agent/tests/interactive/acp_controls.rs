//! ACP agent controls on the virtual terminal before the first prompt: discovery at startup, `/model`,
//! `/acp mode`, and `/thinking`, against the local fake agent with numeric effort values.

#[path = "../../../pi-ai/tests/support/fake_acp_agent.rs"]
mod fake_agent;
#[path = "acp_controls_support.rs"]
mod support;

use pi_ai::ThinkingLevel;
use support::{AGENT_ENV, choose, has_reply, remote_value, serialize, shut_down, start_acp, wait_discovered};

use crate::harness::{Ui, local};

/// The agent process: this binary started with `AGENT_ENV` set runs only this test.
#[test]
fn fake_acp_agent_process() {
    if std::env::var_os(AGENT_ENV).is_some() {
        fake_agent::main();
    }
}

async fn wait_thinking_levels(ui: &Ui) {
    let levels = [ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High];
    ui.wait_until("the agent thinking levels", |ui| ui.session().available_thinking_levels() == levels).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_discovers_the_agent_models_and_model_lists_them_before_a_prompt() {
    local(async {
        let _serial = serialize().await;
        let ui = start_acp(None).await;
        wait_discovered(&ui).await;
        assert!(!has_reply(&ui), "discovery sends no prompt");
        ui.submit("/model").await;
        ui.wait_until("the agent models in /model", |ui| {
            let screen = ui.screen();
            screen.contains("fast") && screen.contains("smart")
        })
        .await;
        eprintln!("ACP model picker:\n{}", ui.screen());
        ui.send("\x1b").await;
        assert!(!has_reply(&ui), "listing models sends no prompt");
        shut_down(&ui).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_mode_sets_the_exact_permission_before_a_prompt_that_reuses_the_session() {
    local(async {
        let _serial = serialize().await;
        let ui = start_acp(None).await;
        wait_discovered(&ui).await;
        ui.submit("/acp mode").await;
        ui.wait_for("Permissions").await;
        choose(&ui, "Permissions").await;
        ui.wait_for("Auto").await;
        choose(&ui, "Auto").await;
        ui.wait_for("Permissions: Auto").await;
        assert_eq!(remote_value(&ui, "permission").as_deref(), Some("auto"));
        assert!(!has_reply(&ui), "/acp mode sends no prompt");

        ui.submit("hello").await;
        ui.wait_for("news=1").await;
        let history = ui.history();
        assert!(history.contains("mode=auto") && history.contains("turn=1"), "{history}");
        ui.wait_until("the turn to end", |ui| !ui.session().is_streaming()).await;
        shut_down(&ui).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_high_sets_effort_three_before_a_prompt_and_the_selector_keeps_it() {
    local(async {
        let _serial = serialize().await;
        let ui = start_acp(None).await;
        wait_thinking_levels(&ui).await;
        assert_eq!(ui.session().thinking_level(), ThinkingLevel::Medium, "the agent starts at effort 2");
        ui.submit("/thinking high").await;
        // The first Enter accepts the argument completion; the second submits the command.
        ui.send("\r").await;
        ui.wait_for("Thinking level: high").await;
        assert_eq!(ui.session().thinking_level(), ThinkingLevel::High);
        assert_eq!(remote_value(&ui, "effortLevel").as_deref(), Some("3"));
        assert!(!has_reply(&ui), "/thinking sends no prompt");

        ui.submit("/thinking").await;
        ui.wait_for("✓ high").await;
        eprintln!("ACP thinking picker:\n{}", ui.screen());
        ui.send("\x1b").await;
        assert_eq!(ui.session().thinking_level(), ThinkingLevel::High, "reopening the selector keeps the level");
        assert_eq!(remote_value(&ui, "effortLevel").as_deref(), Some("3"));

        ui.submit("hello").await;
        ui.wait_for("news=1").await;
        assert!(ui.history().contains("effort=3"), "{}", ui.history());
        ui.wait_until("the turn to end", |ui| !ui.session().is_streaming()).await;
        shut_down(&ui).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_shortcut_sets_the_agent_level_before_saving_it() {
    local(async {
        let _serial = serialize().await;
        let ui = start_acp(None).await;
        wait_thinking_levels(&ui).await;
        assert_eq!(ui.session().thinking_level(), ThinkingLevel::Medium);
        ui.send("\x1b[Z").await;
        ui.wait_until("the next thinking level", |ui| ui.session().thinking_level() == ThinkingLevel::High).await;
        assert_eq!(remote_value(&ui, "effortLevel").as_deref(), Some("3"), "the shortcut must set the agent first");
        assert_eq!(ui.session().settings().default_thinking_level(), Some(ThinkingLevel::High));
        assert!(!has_reply(&ui), "the shortcut sends no prompt");
        ui.submit("/thinking").await;
        ui.wait_for("✓ high").await;
        ui.send("\x1b").await;
        shut_down(&ui).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_thinking_level_changes_nothing() {
    local(async {
        let _serial = serialize().await;
        let ui = start_acp(None).await;
        wait_thinking_levels(&ui).await;
        let (level, default) = (ui.session().thinking_level(), ui.session().settings().default_thinking_level());
        ui.submit("/thinking xhigh").await;
        ui.wait_for("Unknown thinking level \"xhigh\"").await;
        assert_eq!(ui.session().thinking_level(), level);
        assert_eq!(ui.session().settings().default_thinking_level(), default);
        assert_eq!(remote_value(&ui, "effortLevel").as_deref(), Some("2"));
        assert!(!has_reply(&ui));
        shut_down(&ui).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_that_cannot_start_shows_one_error_per_command_and_changes_nothing() {
    local(async {
        let _serial = serialize().await;
        let ui = start_acp(Some("/nonexistent/pi-fake-acp-agent")).await;
        let error = "Error: Cannot start Control test";
        ui.wait_for(error).await;
        let count = |ui: &Ui| ui.history().matches(error).count();
        assert_eq!(count(&ui), 1, "startup shows the error once");
        let level = ui.session().thinking_level();
        ui.submit("/thinking high").await;
        ui.wait_until("the error of /thinking", |ui| count(ui) == 2).await;
        assert_eq!(ui.session().thinking_level(), level);
        assert!(!ui.history().contains("Thinking level: high"), "{}", ui.history());
    })
    .await;
}
