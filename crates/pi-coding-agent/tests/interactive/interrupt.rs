//! Ctrl+Enter (`app.message.interrupt`, pi-rust extension): while the agent works, abort the run
//! and send the queued messages and the editor text as one prompt; otherwise act as Enter.

use pi_ai::providers::faux::FauxResponse;
use pi_ai::{Message, StopReason};
use serde_json::json;

use crate::interactive::harness::{Options, Ui, local, start};

/// Ctrl+Enter as the Kitty keyboard protocol and xterm modifyOtherKeys send it.
const CTRL_ENTER: &str = "\x1b[13;5u";
const CTRL_ENTER_MODIFY_OTHER_KEYS: &str = "\x1b[27;5;13~";

/// The last user message of each model request.
fn sent_prompts(ui: &Ui) -> Vec<String> {
    let last_user = |messages: &[Message]| {
        messages.iter().rev().find_map(|message| match message {
            Message::User(user) => Some(user.content.text()),
            _ => None,
        })
    };
    ui.provider.requests().iter().map(|request| last_user(&request.messages).unwrap_or_default()).collect()
}

fn saved_history(ui: &Ui) -> String {
    std::fs::read_to_string(ui.agent_dir.join("prompt-history.jsonl")).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborts_the_run_and_sends_the_queued_and_typed_messages_as_one_prompt() {
    local(async {
        let responses = vec![FauxResponse::Hang, FauxResponse::text("answer after interrupt")];
        let ui = start(Options { responses, ..Options::default() }).await;
        ui.submit("first question").await;
        ui.wait_until("streaming", |ui| ui.session().is_streaming()).await;
        ui.submit("steer me").await;
        ui.wait_for("Steering: steer me").await;
        ui.wait_for("Ctrl+Enter to send now").await;
        ui.send("later please").await;
        let windows = pi_coding_agent::keybindings::use_windows_keybindings();
        ui.send(if windows { "\x11" } else { "\x1b\r" }).await;
        ui.wait_for("Follow-up: later please").await;

        ui.send("typed now").await;
        ui.send(CTRL_ENTER).await;
        ui.wait_for("Interrupting to send now").await;
        ui.wait_for("answer after interrupt").await;
        assert_eq!(sent_prompts(&ui), ["first question", "steer me\n\nlater please\n\ntyped now"]);
        let aborted = ui.session().messages().iter().any(
            |message| matches!(message, Message::Assistant(assistant) if assistant.stop_reason == StopReason::Aborted),
        );
        assert!(aborted, "the first run was aborted");
        assert!(ui.session().steering_messages().is_empty() && ui.session().follow_up_messages().is_empty());
        let screen = ui.screen();
        assert!(!screen.contains("Steering:") && !screen.contains("Follow-up:"), "{screen}");
        assert_eq!(screen.matches("typed now").count(), 1, "only the sent message, not the editor:\n{screen}");
        let history = saved_history(&ui);
        assert_eq!(history, "\"first question\"\n\"steer me\"\n\"later please\"\n\"typed now\"\n");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_a_long_tool_call() {
    local(async {
        let responses = vec![
            FauxResponse::tool_call("call-1", "bash", json!({"command": "sleep 30"})),
            FauxResponse::text("answer after interrupt"),
        ];
        let ui = start(Options { responses, ..Options::default() }).await;
        ui.submit("run something long").await;
        ui.wait_for("sleep 30").await;
        ui.send("do this instead").await;
        ui.send(CTRL_ENTER_MODIFY_OTHER_KEYS).await;
        ui.wait_for("answer after interrupt").await;
        assert_eq!(sent_prompts(&ui), ["run something long", "do this instead"]);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_commands_like_enter_and_needs_something_to_send_while_streaming() {
    local(async {
        let ui = start(Options { responses: vec![FauxResponse::Hang], ..Options::default() }).await;
        ui.submit("first question").await;
        ui.wait_until("streaming", |ui| ui.session().is_streaming()).await;
        ui.send(CTRL_ENTER).await;
        ui.wait_for("Nothing to send").await;

        ui.send("/session").await;
        ui.send(CTRL_ENTER).await;
        ui.wait_for("Session Info").await;
        ui.send("!echo out-$((40+2))").await;
        ui.send(CTRL_ENTER).await;
        ui.wait_for("out-42").await;
        assert!(ui.session().is_streaming(), "commands do not interrupt the run");
        assert_eq!(ui.provider.requests().len(), 1);
        assert!(!ui.history().contains("Interrupting"), "{}", ui.history());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submits_like_enter_while_idle() {
    local(async {
        let ui = start(Options { responses: vec![FauxResponse::text("idle answer")], ..Options::default() }).await;
        ui.send(CTRL_ENTER).await;
        ui.settle(100).await;
        assert!(ui.provider.requests().is_empty());
        assert!(!ui.history().contains("Nothing to send"), "{}", ui.history());

        ui.send("hello while idle").await;
        ui.send(CTRL_ENTER_MODIFY_OTHER_KEYS).await;
        ui.wait_for("idle answer").await;
        assert_eq!(sent_prompts(&ui), ["hello while idle"]);
        assert_eq!(saved_history(&ui), "\"hello while idle\"\n");
        assert!(!ui.history().contains("Interrupting"), "{}", ui.history());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sends_the_messages_queued_during_compaction_with_the_typed_text() {
    local(async {
        let responses = vec![
            FauxResponse::text("first answer"),
            FauxResponse::text("second answer"),
            FauxResponse::Hang,
            FauxResponse::text("answer after interrupt"),
        ];
        let settings = json!({"compaction": {"keepRecentTokens": 6}});
        let ui = start(Options { responses, settings, ..Options::default() }).await;
        for (question, answer) in [("first question", "first answer"), ("second question", "second answer")] {
            ui.submit(question).await;
            ui.wait_for(answer).await;
            ui.wait_until("idle", |ui| ui.session().is_idle()).await;
        }
        ui.submit("/compact").await;
        ui.wait_until("compacting", |ui| ui.session().is_compacting()).await;
        ui.submit("queued during compaction").await;
        ui.wait_for("Queued message for after compaction").await;

        ui.send("typed now").await;
        ui.send(CTRL_ENTER).await;
        ui.wait_for("answer after interrupt").await;
        ui.wait_for("Compaction cancelled").await;
        ui.settle(200).await;
        let prompts = sent_prompts(&ui);
        assert_eq!(prompts.len(), 4, "the compaction queue is not sent again: {prompts:?}");
        assert_eq!(prompts[3], "queued during compaction\n\ntyped now");
    })
    .await;
}
