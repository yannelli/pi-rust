//! App keybindings and slash commands.

use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, local, start};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_keys_cycle_thinking_and_models_and_toggle_views() {
    local(async {
        let ui = start(Options::default()).await;
        let before = ui.session().thinking_level();
        ui.send("\x1b[Z").await;
        ui.wait_for("Thinking level:").await;
        assert_ne!(ui.session().thinking_level(), before);

        ui.send("\x10").await;
        ui.wait_for("Switched to").await;
        assert_eq!(ui.session().model().unwrap().id, "other");

        ui.send("\x0f").await;
        ui.wait_for("Tool output: expanded").await;
        ui.send("\x14").await;
        ui.wait_for("Thinking blocks: hidden").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_commands_show_info_and_quit() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("/hotkeys").await;
        ui.wait_for("Keyboard Shortcuts").await;
        ui.submit("/name demo session").await;
        ui.wait_for("Session name set: demo session").await;
        assert_eq!(ui.session().session_name().as_deref(), Some("demo session"));
        ui.submit("/session").await;
        ui.wait_for("Session Info").await;
        assert_eq!(ui.provider.requests().len(), 0, "commands do not prompt the model");
        ui.submit("/quit").await;
        ui.wait_until("exit", |ui| ui.mode.is_exited()).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_menu_completes_command_names() {
    local(async {
        let ui = start(Options::default()).await;
        ui.send("/hotk").await;
        ui.wait_for("Show all keyboard shortcuts").await;
        ui.send("\t").await;
        ui.wait_until("completion", |ui| ui.screen().contains("/hotkeys")).await;
        ui.send("\r").await;
        ui.wait_for("Keyboard Shortcuts").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_session_clears_the_chat() {
    local(async {
        let ui = start(Options { responses: vec![FauxResponse::text("first answer")], ..Options::default() }).await;
        ui.submit("question one").await;
        ui.wait_for("first answer").await;
        let before = ui.session().session_id();
        ui.submit("/new").await;
        ui.wait_for("New session started").await;
        assert_ne!(ui.session().session_id(), before);
        assert!(!ui.screen().contains("first answer"), "{}", ui.screen());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escape_clears_a_long_draft_without_quitting_or_submitting() {
    local(async {
        let ui = start(Options::default()).await;
        let draft = "discard this draft ".repeat(40);
        ui.send(&draft).await;
        ui.wait_for("discard this draft").await;
        ui.send("\x1b").await;
        ui.wait_until("cleared editor", |ui| !ui.screen().contains("discard this draft")).await;
        assert!(!ui.mode.is_exited());
        assert!(ui.provider.requests().is_empty());
        let bindings = pi_coding_agent::keybindings::keybindings();
        let undo = match bindings["tui.editor.undo"].default_keys[0].as_str() {
            "ctrl+z" => "\x1a",
            "alt+z" => "\x1bz",
            "ctrl+-" => "\x1f",
            other => panic!("unexpected undo binding: {other}"),
        };
        ui.send(undo).await;
        ui.wait_until("undo restores draft", |ui| ui.screen().contains("discard this draft")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_clears_the_editor_and_twice_exits() {
    local(async {
        let ui = start(Options::default()).await;
        ui.send("draft text").await;
        ui.wait_for("draft text").await;
        ui.send("\x03").await;
        ui.wait_until("cleared editor", |ui| !ui.screen().contains("draft text")).await;
        ui.send("\x03").await;
        ui.send("\x03").await;
        ui.wait_until("exit", |ui| ui.mode.is_exited()).await;
    })
    .await;
}
