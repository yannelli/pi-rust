//! `keybindings.json` and `/reload`. A separate binary, since keybindings are process-wide.

#[path = "interactive/harness.rs"]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Options, local, start};
use pi_coding_agent::keybindings::keybindings;
use pi_coding_agent::platform::{PlatformEnv, app_key_defaults};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_applies_app_actions_from_keybindings_json() {
    local(async {
        let ui = start(Options::default()).await;
        std::fs::write(ui.agent_dir.join("keybindings.json"), r#"{"app.session.new": "ctrl+y"}"#).unwrap();
        ui.send("\x19").await;
        ui.settle(200).await;
        assert!(!ui.history().contains("New session started"), "unbound before the reload");
        ui.submit("/reload").await;
        ui.wait_for("Reloaded keybindings").await;
        ui.send("\x19").await;
        ui.wait_for("New session started").await;
    })
    .await;
}

#[test]
fn platform_key_defaults_match_the_installed_definitions() {
    let definitions = keybindings();
    for (id, keys) in app_key_defaults(&PlatformEnv::from_process()) {
        let definition = definitions.get(id).unwrap_or_else(|| panic!("{id} is not defined"));
        assert_eq!(definition.default_keys, keys, "{id}");
    }
}
