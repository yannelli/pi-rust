//! The `/extensions` browser on a virtual terminal: marketplace install and remove against a
//! signed local release server, toggling a JS extension, and inline marketplace errors.

#[path = "extensions_browser_support.rs"]
pub(in crate::interactive) mod support;

use serde_json::json;

use crate::packages::marketplace::host_target;
use crate::settings::schema::PackageSource;
use crate::settings::{ResourceKind, SettingsScope};
use support::{BINARY, MockMarketplace, Ui, local, start};

const CTRL_I: &str = "\x1b[105;5u";
const CTRL_D: &str = "\x1b[100;5u";
const CTRL_R: &str = "\x1b[114;5u";
const CTRL_U: &str = "\x1b[117;5u";
const ESCAPE: &str = "\x1b[27u";
const SOURCE: &str = "marketplace:todo";

const HELLO: &str = r#"export default function (pi) {
	pi.registerCommand("hello-browser", { description: "Say hello", handler: async () => {} });
}
"#;

fn has_source(ui: &Ui) -> bool {
    let packages = ui.session().settings().packages(SettingsScope::Global);
    packages.iter().any(|package| matches!(package, PackageSource::Source(source) if source == SOURCE))
}

fn todo_dir(ui: &Ui) -> std::path::PathBuf {
    ui.agent_dir.join("marketplace/packages/todo")
}

fn hello_loaded(ui: &Ui) -> bool {
    ui.loaded_extensions().iter().any(|path| path.ends_with("hello.ts"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installs_and_removes_a_marketplace_extension() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1", &["todo"], "binary-v1"));
        let ui = start(&[]).await;
        ui.mode.show_extensions_browser(ui.package_manager(), mock.marketplace(&ui.agent_dir, false));
        ui.wait_for("Marketplace (1)").await;

        ui.send("\t").await;
        ui.wait_for("The todo extension").await;
        ui.send("\r").await;
        ui.wait_for("Actions").await;
        ui.send("\r").await;
        ui.wait_until("todo installed", |ui| todo_dir(ui).join("package.json").is_file() && has_source(ui)).await;
        ui.wait_for("Extension change saved.").await;
        ui.wait_for("Ctrl+R to reload").await;
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(todo_dir(&ui).join("package.json")).unwrap()).unwrap();
        assert_eq!(manifest["name"], "@pi-rust-extensions/todo");
        assert_eq!(manifest["pi"]["native"][0]["args"], json!(["todo"]));
        assert_eq!(manifest["piMarketplace"]["tag"], "v1");

        ui.send(ESCAPE).await;
        ui.send("\t").await;
        ui.wait_for("[Installed (").await;
        ui.send(CTRL_D).await;
        ui.wait_for("Remove marketplace:todo? y confirm").await;
        ui.send("y").await;
        ui.wait_until("todo removed", |ui| !todo_dir(ui).exists() && !has_source(ui)).await;
        let target = host_target().unwrap();
        assert!(!ui.agent_dir.join(format!("marketplace/bin/v1/{target}/{BINARY}")).exists());

        ui.send(ESCAPE).await;
        ui.wait_until("browser closed", |ui| !ui.screen().contains("Marketplace (1)")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_i_installs_from_the_marketplace_tab() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1", &["todo", "notes"], "binary-v1"));
        let ui = start(&[]).await;
        ui.mode.show_extensions_browser(ui.package_manager(), mock.marketplace(&ui.agent_dir, false));
        ui.wait_for("Marketplace (2)").await;
        ui.send("\t").await;
        ui.send("todo").await;
        ui.wait_until("todo only", |ui| !ui.screen().contains("The notes extension")).await;
        ui.send(CTRL_I).await;
        ui.wait_until("todo installed", |ui| todo_dir(ui).join("package.json").is_file() && has_source(ui)).await;
        assert!(!ui.agent_dir.join("marketplace/packages/notes").exists());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_u_updates_an_installed_marketplace_extension() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1", &["todo"], "binary-v1"));
        let ui = start(&[]).await;
        mock.marketplace(&ui.agent_dir, false).install("todo", None).await.unwrap();
        let manager = ui.package_manager();
        manager.add_source_to_settings(SOURCE, false).unwrap();
        mock.publish(&mock.release("v2", &["todo"], "binary-v2"));
        ui.mode.show_extensions_browser(manager, mock.marketplace(&ui.agent_dir, false));
        ui.wait_for("update available").await;
        ui.send(CTRL_U).await;
        let tag = |ui: &Ui| {
            let manifest = std::fs::read(todo_dir(ui).join("package.json")).unwrap_or_default();
            serde_json::from_slice::<serde_json::Value>(&manifest).unwrap_or_default()["piMarketplace"]["tag"].clone()
        };
        ui.wait_until("todo on v2", |ui| tag(ui) == "v2").await;
        ui.wait_for("Extension change saved.").await;
        assert!(has_source(&ui));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_a_js_extension_unloads_it_after_reload() {
    local(async {
        let mock = MockMarketplace::start().await;
        let ui = start(&[("extensions/hello.ts", HELLO)]).await;
        ui.wait_until("hello loaded", hello_loaded).await;
        ui.mode.show_extensions_browser(ui.package_manager(), mock.marketplace(&ui.agent_dir, true));
        ui.wait_until("rows loaded", |ui| !ui.screen().contains("Loading installed extensions")).await;

        ui.send("hello").await;
        ui.send("\r").await;
        ui.wait_for("installed, enabled").await;
        ui.send("\r").await;
        ui.wait_for("Extension change saved.").await;
        let disabled = [SettingsScope::Global, SettingsScope::Project].into_iter().any(|scope| {
            let entries = ui.session().settings().resource_paths(ResourceKind::Extensions, scope);
            entries.iter().any(|entry| entry.starts_with(['-', '!']) && entry.ends_with("hello.ts"))
        });
        assert!(disabled, "settings do not disable hello.ts");

        ui.send(CTRL_R).await;
        ui.wait_until("hello unloaded", |ui| !hello_loaded(ui)).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_index_signature_shows_on_the_marketplace_tab() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1", &["todo"], "binary-v1"));
        mock.put("/index.json.sig", mock.signature(b"other bytes"));
        let ui = start(&[]).await;
        ui.mode.show_extensions_browser(ui.package_manager(), mock.marketplace(&ui.agent_dir, false));
        ui.send("\t").await;
        ui.wait_for("Marketplace unavailable: Marketplace index signature is not valid").await;
        ui.wait_for("Installed extensions still work").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checksum_mismatch_fails_the_install_inline() {
    local(async {
        let mock = MockMarketplace::start().await;
        let mut index = mock.release("v1", &["todo"], "binary-v1");
        index["binaries"][BINARY][host_target().unwrap()]["sha256"] = json!("0".repeat(64));
        mock.publish(&index);
        let ui = start(&[]).await;
        ui.mode.show_extensions_browser(ui.package_manager(), mock.marketplace(&ui.agent_dir, false));
        ui.wait_for("Marketplace (1)").await;
        ui.send("\t").await;
        ui.send(CTRL_I).await;
        ui.wait_for("Extension change failed").await;
        ui.wait_for("SHA-256").await;
        assert!(!todo_dir(&ui).exists());
        assert!(!has_source(&ui));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_mode_lists_installed_extensions() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1", &["todo"], "binary-v1"));
        let ui = start(&[]).await;
        mock.marketplace(&ui.agent_dir, false).install("todo", None).await.unwrap();
        let manager = ui.package_manager();
        manager.add_source_to_settings(SOURCE, false).unwrap();
        let requests = mock.server.requests().len();
        ui.mode.show_extensions_browser(manager, mock.marketplace(&ui.agent_dir, true));
        ui.wait_for("marketplace:todo").await;
        ui.send("\t").await;
        ui.wait_for("Marketplace unavailable: Cannot reach the marketplace in offline mode").await;
        assert_eq!(mock.server.requests().len(), requests);
    })
    .await;
}
