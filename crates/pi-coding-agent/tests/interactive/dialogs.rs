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
async fn fast_command_toggles_and_saves_fast_mode() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("/fast").await;
        ui.wait_for("Fast mode on (the current model has no fast mode)").await;
        assert!(ui.session().fast_mode() && ui.session().settings().fast_mode());
        ui.submit("/fast off").await;
        ui.send("\r").await;
        ui.wait_for("Fast mode off").await;
        assert!(!ui.session().fast_mode() && !ui.session().settings().fast_mode());
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
async fn settings_disable_and_re_enable_a_provider_in_the_live_registry() {
    local(async {
        let ui = start(Options::default()).await;
        let local_models = |ui: &crate::harness::Ui| ui.session().model_runtime().get_model("local", "other").is_some();
        assert!(local_models(&ui));
        ui.submit("/settings").await;
        ui.wait_for("Auto-compact").await;
        ui.send("disabled providers").await;
        ui.wait_until("filtered list", |ui| !ui.screen().contains("Auto-compact")).await;
        ui.wait_for("none").await;
        ui.send("\r").await;
        ui.wait_for("enabled").await;
        ui.send("local").await;
        ui.wait_until("filtered providers", |ui| !ui.screen().contains("(1/")).await;
        ui.send("\r").await;
        ui.wait_until("provider disabled", |ui| ui.session().settings().disabled_providers() == ["local"]).await;
        ui.wait_for("disabled").await;
        assert!(!ui.screen().contains("Disabled providers"), "submenu stays open: {}", ui.screen());
        assert!(!local_models(&ui));
        assert!(ui.session().model_runtime().available_models().iter().all(|model| model.provider != "local"));
        assert_eq!(ui.session().settings().global_settings().disabled_providers, Some(vec!["local".to_string()]));
        ui.send("\x1b").await;
        ui.wait_for("1 disabled").await;

        ui.send("\r").await;
        ui.wait_for("enabled").await;
        ui.send("local").await;
        ui.wait_until("filtered providers", |ui| !ui.screen().contains("(1/")).await;
        ui.send("\r").await;
        ui.wait_until("provider enabled", |ui| ui.session().settings().disabled_providers().is_empty()).await;
        ui.send("\x1b").await;
        ui.wait_for("none").await;
        assert!(local_models(&ui));
        assert!(ui.session().model_runtime().has_configured_auth("local"));
        let models = std::fs::read_to_string(ui.agent_dir.join("models.json")).unwrap();
        assert!(models.contains("test-key"));
        assert_eq!(ui.session().settings().global_settings().disabled_providers, None);
        ui.send("\x1b").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_change_retry_values_from_their_defaults() {
    local(async {
        let ui = start(Options::default()).await;
        let session = ui.session();
        let settings = session.settings();
        assert_eq!(settings.retry_settings().max_retries, 3);
        assert_eq!(settings.provider_retry_settings().timeout_ms, None);
        ui.submit("/settings").await;
        ui.wait_for("Auto-compact").await;
        ui.send("Max retries").await;
        ui.wait_until("filtered list", |ui| !ui.screen().contains("Auto-compact")).await;
        ui.send("\r").await;
        ui.wait_until("max retries saved", |ui| ui.session().settings().retry_settings().max_retries == 5).await;
        ui.send("\x1b").await;
        ui.wait_until("settings closed", |ui| !ui.screen().contains("Max retries")).await;

        ui.submit("/settings").await;
        ui.wait_for("Auto-compact").await;
        ui.send("Provider request timeout").await;
        ui.wait_until("filtered list", |ui| !ui.screen().contains("Auto-compact")).await;
        ui.wait_for("default").await;
        ui.send("\r").await;
        let timeout = |ui: &crate::harness::Ui| ui.session().settings().provider_retry_settings().timeout_ms;
        ui.wait_until("timeout saved", |ui| timeout(ui) == Some(30_000)).await;
        ui.send("\x1b").await;

        let retry = ui.session().settings().global_settings().retry.unwrap();
        assert_eq!(retry.max_retries, Some(5.0));
        assert_eq!(retry.provider.unwrap().timeout_ms, Some(30_000.0));
        let defaults = ui.session().settings().retry_settings();
        assert_eq!((defaults.enabled, defaults.base_delay_ms, defaults.max_agent_delay_ms), (true, 2_000, 60_000));
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
