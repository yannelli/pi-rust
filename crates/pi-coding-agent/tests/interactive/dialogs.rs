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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statusline_editor_applies_a_preset_and_restores_the_prompt() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("/statusline").await;
        ui.wait_for("Status line · global settings").await;
        ui.send("\x1b[B").await;
        ui.send("\r").await;
        ui.send("compact").await;
        ui.send("\r").await;
        ui.wait_until("status line saved", |ui| {
            ui.session().settings().status_line().is_some_and(|settings| settings.preset.as_deref() == Some("compact"))
        })
        .await;
        ui.send("\x1b").await;
        ui.wait_until("status line editor closed", |ui| !ui.screen().contains("Status line · global settings")).await;
        ui.wait_for("work · tiny").await;
        ui.send("typed after statusline").await;
        ui.wait_for("typed after statusline").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_module_layout_hides_the_footer_but_keeps_extension_statuses() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use pi_coding_agent::interactive::components::FooterComponent;
    use pi_coding_agent::interactive::footer_data::FooterData;
    use pi_coding_agent::interactive::status_line::StatusLineConfig;
    use pi_tui::Component;

    local(async {
        let ui = start(Options::default()).await;
        let data = Rc::new(RefCell::new(FooterData::new(&ui.cwd())));
        let mut footer = FooterComponent::new(ui.session(), data.clone());
        let settings = serde_json::from_value(serde_json::json!({"segments": []})).unwrap();
        footer.set_status_line(StatusLineConfig::from_settings(&settings, None), Rc::new(|| {}));
        assert!(footer.render(100).is_empty());
        data.borrow_mut().set_extension_status("test", Some("extension status"));
        assert_eq!(footer.render(100).len(), 1);
        assert!(footer.render(100)[0].contains("extension status"));
    })
    .await;
}

struct OutsideAuth;

#[async_trait::async_trait]
impl pi_coding_agent::agent_session::ProviderAuth for OutsideAuth {
    async fn check(&self, _stored: Option<&pi_ai::auth::Credential>) -> bool {
        false
    }

    async fn resolve(
        &self,
        _stored: Option<&pi_ai::auth::Credential>,
    ) -> Result<Option<pi_ai::auth::ResolvedAuth>, String> {
        Ok(None)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_explains_providers_configured_outside_pi() {
    local(async {
        let ui = start(Options::default()).await;
        ui.session().model_runtime().register_provider_auth("outside-cloud", Some(std::sync::Arc::new(OutsideAuth)));
        ui.submit("/login outside-cloud").await;
        ui.wait_for("Authentication is configured outside pi.").await;
        assert!(ui.screen().contains("outside-cloud setup"), "{}", ui.screen());
        ui.send("\x1b").await;
        ui.wait_until("dialog closed", |ui| !ui.screen().contains("is configured outside")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renaming_the_current_session_from_resume_renames_the_live_session() {
    local(async {
        let responses = vec![pi_ai::providers::faux::FauxResponse::text("first answer")];
        let ui = start(Options { responses, ..Options::default() }).await;
        ui.submit("first question").await;
        ui.wait_for("first answer").await;
        ui.submit("/name Before").await;
        ui.wait_until("named", |ui| ui.session().session_name().as_deref() == Some("Before")).await;
        ui.submit("/resume").await;
        ui.wait_for("Resume Session").await;
        ui.settle(300).await;
        ui.send("\x12").await;
        ui.wait_for("Rename Session").await;
        // The input starts with the current name: Ctrl+E then Ctrl+U clears it.
        ui.send("\x05").await;
        ui.send("\x15").await;
        ui.send("After").await;
        ui.send("\r").await;
        ui.wait_until("renamed", |ui| ui.session().session_name().as_deref() == Some("After")).await;
        let file = ui.session().session_file().unwrap();
        let stored = pi_coding_agent::session::SessionManager::open(&file).unwrap().session_name();
        assert_eq!(stored.as_deref(), Some("After"));
        ui.send("\x1b").await;
    })
    .await;
}
