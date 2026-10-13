//! Dialogs opened from slash commands replace the editor until chosen or dismissed.

use crate::harness::{Options, local, start};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_selector_switches_the_model() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("/model").await;
        ui.wait_for("Only showing models from configured providers").await;
        ui.send("other").await;
        ui.send("\r").await;
        ui.wait_for("Model: other").await;
        assert_eq!(ui.session().model().unwrap().id, "other");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_command_with_an_exact_reference_skips_the_selector() {
    local(async {
        let ui = start(Options::default()).await;
        // Enter first takes the argument completion, as upstream; the second Enter submits.
        ui.submit("/thinking high").await;
        ui.send("\r").await;
        ui.wait_for("Thinking level: high").await;
        assert_eq!(ui.session().thinking_level().as_str(), "high");
        ui.submit("/model local/other").await;
        ui.send("\r").await;
        ui.wait_for("Model: other").await;
        ui.submit("/thinking high").await;
        ui.send("\r").await;
        ui.wait_for("Unknown thinking level \"high\". Available levels: off.").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_open_and_escape_restores_the_editor() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("/settings").await;
        ui.wait_for("Auto-compact").await;
        ui.send("\x1b").await;
        ui.wait_until("settings closed", |ui| !ui.screen().contains("Auto-compact")).await;
        ui.send("typed after").await;
        ui.wait_for("typed after").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_change_the_hide_thinking_setting() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("/settings").await;
        ui.wait_for("Auto-compact").await;
        ui.send("hide thinking").await;
        ui.wait_until("filtered list", |ui| !ui.screen().contains("Auto-compact")).await;
        ui.send("\r").await;
        ui.wait_until("setting saved", |ui| ui.session().settings().hide_thinking_block()).await;
        ui.send("\x1b").await;
    })
    .await;
}
