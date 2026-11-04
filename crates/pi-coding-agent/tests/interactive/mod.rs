//! The interactive mode end to end: a virtual terminal, the faux provider, and the real UI loop.

mod acp;
mod components;
mod components_b;
mod components_c;
mod context;
mod dialogs;
mod diff;
mod extension_dialogs;
mod extensions;
mod gaps;
pub(crate) mod harness;
mod host_additions;
mod init;
mod keys;
mod notify;
mod prompt_history;
mod quit;
mod retry;
mod review;
mod run_limits;
mod startup;
mod undo;
pub(crate) mod watchdog;

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
async fn send_now_moves_queued_follow_ups_to_steering() {
    local(async {
        let ui = start(Options { responses: vec![FauxResponse::Hang], ..Options::default() }).await;
        ui.submit("first question").await;
        ui.wait_until("streaming", |ui| ui.session().is_streaming()).await;
        ui.send("later please").await;
        let windows = pi_coding_agent::keybindings::use_windows_keybindings();
        ui.send(if windows { "\x11" } else { "\x1b\r" }).await;
        ui.wait_for("Follow-up: later please").await;
        let send_now = if windows { "Alt+S" } else { "Alt+Down" };
        ui.wait_for(&format!("{send_now} to send follow-ups now")).await;
        // Send now is alt+s with Windows keybindings and alt+down elsewhere.
        ui.send(if windows { "\x1bs" } else { "\x1b[1;3B" }).await;
        ui.wait_for("Steering: later please").await;
        ui.wait_for("Sending 1 queued message after the current turn").await;
        assert_eq!(ui.session().steering_messages(), ["later please"]);
        assert!(ui.session().follow_up_messages().is_empty());
        assert!(!ui.screen().contains("to send follow-ups now"), "{}", ui.screen());
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
