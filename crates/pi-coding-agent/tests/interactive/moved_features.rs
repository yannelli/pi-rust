//! The startup offer for the status line and iTerm2 extensions, installs from a signed local
//! marketplace, offline failures, and the `/statusline` hint, on a virtual terminal.

use std::time::Duration;

use serde_json::json;

use crate::interactive::extensions_view::tests::support::{MockMarketplace, Ui, local, start, start_with};
use crate::packages::moved_features::{MigrationChoices, MovedFeature};
use crate::settings::SettingsScope;
use crate::settings::schema::PackageSource;

const STATUS_LINE_OFFER: &str = "The status line is now an extension. Install marketplace:statusline?";
const ITERM2_OFFER: &str = "The iTerm2 integration is now an extension. Install marketplace:iterm2?";
const DOWN: &str = "\x1b[B";

fn has_source(ui: &Ui, source: &str) -> bool {
    let packages = ui.session().settings().packages(SettingsScope::Global);
    packages.iter().any(|package| matches!(package, PackageSource::Source(found) if found == source))
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_now_asks_once_per_run_and_do_not_ask_again_persists() {
    local(async {
        let ui = start_with(&[], json!({"statusLine": {"preset": "pills"}})).await;
        ui.wait_for(STATUS_LINE_OFFER).await;
        ui.wait_for("Do not ask again").await;
        ui.send(DOWN).await;
        ui.send("\r").await;
        ui.wait_until("offer closed", |ui| !ui.screen().contains(STATUS_LINE_OFFER)).await;
        assert!(!MigrationChoices::path(&ui.agent_dir).exists(), "Not now saves nothing");

        ui.mode.handle_reload_command();
        settle().await;
        assert!(!ui.screen().contains(STATUS_LINE_OFFER), "a reload does not ask again");

        ui.mode.check_moved_features();
        ui.wait_for(STATUS_LINE_OFFER).await;
        ui.send(DOWN).await;
        ui.send(DOWN).await;
        ui.send("\r").await;
        ui.wait_until("choice saved", |ui| {
            MigrationChoices::load(&ui.agent_dir).is_dismissed(MovedFeature::StatusLine)
        })
        .await;
        ui.wait_until("offer closed", |ui| !ui.screen().contains(STATUS_LINE_OFFER)).await;

        ui.mode.check_moved_features();
        settle().await;
        assert!(!ui.screen().contains(STATUS_LINE_OFFER), "a dismissed offer stays hidden");
        let settings = ui.session().settings().global_settings();
        assert!(settings.packages.unwrap_or_default().is_empty(), "settings.json keeps upstream keys only");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_adds_the_marketplace_extension_and_reloads() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1", &["iterm2", "statusline"], "binary-v1"));
        let ui = start(&[]).await;
        ui.session().settings().set_global_value("terminal", Some(json!({"iterm2": true})));

        ui.mode.offer_moved_features(ui.package_manager(), mock.marketplace(&ui.agent_dir, false));
        ui.wait_for(ITERM2_OFFER).await;
        ui.send("\r").await;
        let package = ui.agent_dir.join("marketplace/packages/iterm2/package.json");
        ui.wait_until("iterm2 installed", |ui| package.is_file() && has_source(ui, "marketplace:iterm2")).await;
        ui.wait_for("Reloaded keybindings, extensions").await;
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&package).unwrap()).unwrap();
        assert_eq!(manifest["pi"]["native"][0]["args"], json!(["iterm2"]));

        settle().await;
        ui.mode.offer_moved_features(ui.package_manager(), mock.marketplace(&ui.agent_dir, false));
        settle().await;
        assert!(!ui.screen().contains(ITERM2_OFFER), "an installed extension is not offered");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_install_shows_the_install_command() {
    local(async {
        let mock = MockMarketplace::start().await;
        let ui = start(&[]).await;
        ui.session().settings().set_global_value("terminal", Some(json!({"iterm2": {"badge": true}})));

        ui.mode.offer_moved_features(ui.package_manager(), mock.marketplace(&ui.agent_dir, true));
        ui.wait_for(ITERM2_OFFER).await;
        ui.send("\r").await;
        ui.wait_for("Could not install marketplace:iterm2:").await;
        ui.wait_for("pi install marketplace:iterm2").await;
        assert!(!has_source(&ui, "marketplace:iterm2"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_commands_show_the_install_hint() {
    local(async {
        let ui = start(&[]).await;
        ui.mode.handle_submit("/statusline compact");
        ui.wait_for("The status line is now an extension. Install it with").await;
        ui.wait_for("pi install marketplace:statusline").await;
        ui.mode.handle_submit("/terminal-setup");
        ui.wait_for("pi install marketplace:iterm2").await;
    })
    .await;
}
