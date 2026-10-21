//! The interactive mode end to end: a virtual terminal, the faux provider, and the real UI loop.

mod context;
mod dialogs;
mod diff;
mod extension_dialogs;
mod extensions;
mod gaps;
mod harness;
mod init;
mod keys;
mod notify;
#[path = "../offline_guard/mod.rs"]
mod offline_guard;
mod prompt_history;
mod review;
mod run_limits;
mod startup;
mod undo;

use harness::{Options, local, start};
use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_a_markdown_response_and_updates_the_footer() {
    local(async {
        let ui =
            start(Options { responses: vec![FauxResponse::text("Hello **world** from faux")], ..Options::default() })
                .await;
        ui.submit("hi there").await;
        ui.wait_for("Hello world from faux").await;
        let screen = ui.screen();
        assert!(screen.contains("hi there"), "{screen}");
        ui.wait_until("token counts", |ui| ui.screen().contains('↑')).await;
        assert!(ui.screen().contains("work"), "the footer names the cwd:\n{}", ui.screen());
        assert_eq!(ui.provider.requests().len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renders_read_and_bash_tool_calls() {
    local(async {
        let responses = vec![
            FauxResponse::tool_call("call-1", "read", json!({"path": "notes.txt"})),
            FauxResponse::tool_call("call-2", "bash", json!({"command": "echo tool-output-ok"})),
            FauxResponse::text("All done"),
        ];
        let files = vec![("notes.txt", "alpha\nbeta\n")];
        let ui = start(Options { responses, files, ..Options::default() }).await;
        ui.submit("use tools").await;
        ui.wait_for("All done").await;
        let screen = ui.screen();
        assert!(screen.contains("notes.txt"), "{screen}");
        assert!(screen.contains("echo tool-output-ok"), "{screen}");
        assert!(screen.contains("tool-output-ok"), "{screen}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queues_steering_and_follow_ups_and_restores_them_on_abort() {
    local(async {
        let ui = start(Options { responses: vec![FauxResponse::Hang], ..Options::default() }).await;
        ui.submit("first question").await;
        ui.wait_until("streaming", |ui| ui.session().is_streaming()).await;
        ui.submit("steer me").await;
        ui.wait_for("Steering: steer me").await;
        ui.send("later please").await;
        // The follow-up key is ctrl+q with Windows keybindings and alt+enter elsewhere.
        let windows = pi_coding_agent::keybindings::use_windows_keybindings();
        ui.send(if windows { "\x11" } else { "\x1b\r" }).await;
        ui.wait_for("Follow-up: later please").await;
        ui.send("\x1b").await;
        ui.wait_until("the abort", |ui| !ui.session().is_streaming()).await;
        ui.wait_until("queue cleared", |ui| !ui.screen().contains("Steering:")).await;
        let screen = ui.screen();
        assert!(screen.contains("steer me") && screen.contains("later please"), "{screen}");
        assert!(ui.session().steering_messages().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_bang_commands_in_the_chat() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("!echo bang-ran").await;
        ui.wait_for("bang-ran").await;
        assert!(ui.screen().contains("$ echo bang-ran") || ui.screen().contains("echo bang-ran"));
        assert_eq!(ui.provider.requests().len(), 0, "a bash command does not prompt the model");
    })
    .await;
}
