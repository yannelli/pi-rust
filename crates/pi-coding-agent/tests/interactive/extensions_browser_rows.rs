//! Installed rows of the `/extensions` browser: a package with one extension is one row, a
//! package with several keeps a row per extension, and each scope is its own installation.

use serde_json::json;

use super::super::component::BrowserAction;
use super::super::data::{BrowserBackend, Snapshot};
use super::SOURCE;
use super::support::{MockMarketplace, Ui, local, start};
use crate::resources::SourceScope;
use crate::settings::SettingsScope;
use crate::settings::schema::PackageSource;

const NATIVE_PATH: &str = "native:@pi-rust-extensions/todo/todo";

fn backend(ui: &Ui, mock: &MockMarketplace) -> BrowserBackend {
    BrowserBackend {
        manager: ui.package_manager(),
        marketplace: mock.marketplace(&ui.agent_dir, true),
        session: ui.session(),
    }
}

fn ids(snapshot: &Snapshot) -> Vec<&str> {
    snapshot.rows.iter().map(|row| row.id.as_str()).collect()
}

fn enabled(snapshot: &Snapshot, id: &str) -> Option<bool> {
    snapshot.rows.iter().find(|row| row.id == id).and_then(|row| row.enabled)
}

async fn toggle(backend: &BrowserBackend, snapshot: &Snapshot, id: &str) {
    let target = snapshot.targets.get(id).cloned();
    backend.apply(BrowserAction::Toggle(id.into()), target).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marketplace_extension_is_one_row_that_toggles() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1.0.0", &["todo"], "binary-v1"));
        let ui = start(&[("extensions/hello.ts", "export default function () {}\n")]).await;
        mock.marketplace(&ui.agent_dir, false).install("todo", None).await.unwrap();
        let manager = ui.package_manager();
        manager.add_source_to_settings(SOURCE, false).unwrap();
        ui.mode.show_extensions_browser(manager, mock.marketplace(&ui.agent_dir, true));
        ui.wait_for("marketplace:todo (User)").await;
        let screen = ui.screen();
        assert!(screen.contains("[Installed (2)]"), "{screen}");
        assert!(screen.contains("hello.ts"), "{screen}");
        assert!(!screen.contains(NATIVE_PATH), "{screen}");

        ui.send("todo").await;
        ui.send("\r").await;
        ui.wait_for(&format!("Extension: {NATIVE_PATH}")).await;
        ui.send("\r").await;
        ui.wait_for("Extension change saved.").await;
        ui.wait_for("installed, disabled").await;
        let packages = ui.session().settings().packages(SettingsScope::Global);
        let disabled = packages.iter().any(|package| match package {
            PackageSource::Filtered(filter) => {
                filter.source == SOURCE && filter.extensions == Some(vec![format!("-{NATIVE_PATH}")])
            }
            PackageSource::Source(_) => false,
        });
        assert!(disabled, "{packages:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_package_with_two_extensions_toggles_each_one() {
    local(async {
        let mock = MockMarketplace::start().await;
        let ui = start(&[]).await;
        let package = ui.root.path().join("multi");
        std::fs::create_dir_all(&package).unwrap();
        let manifest = json!({"name": "multi", "pi": {"extensions": ["a.ts", "b.ts"]}});
        std::fs::write(package.join("package.json"), manifest.to_string()).unwrap();
        for name in ["a.ts", "b.ts"] {
            std::fs::write(package.join(name), "export default function () {}\n").unwrap();
        }
        let manager = ui.package_manager();
        manager.add_source_to_settings(&package.display().to_string(), false).unwrap();
        let backend = backend(&ui, &mock);
        let snapshot = backend.snapshot().await.unwrap();
        let rows = ids(&snapshot);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[0].starts_with("package:User:"), "{rows:?}");
        let child = |name: &str| rows.iter().find(|id| id.ends_with(name)).map(|id| id.to_string()).unwrap();
        let (a, b) = (child("a.ts"), child("b.ts"));

        toggle(&backend, &snapshot, &a).await;
        let snapshot = backend.snapshot().await.unwrap();
        assert_eq!(enabled(&snapshot, &a), Some(false));
        assert_eq!(enabled(&snapshot, &b), Some(true));
        assert_eq!(enabled(&snapshot, rows[0]), Some(true));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_source_in_two_scopes_is_two_rows() {
    local(async {
        let mock = MockMarketplace::start().await;
        mock.publish(&mock.release("v1.0.0", &["todo"], "binary-v1"));
        let ui = start(&[]).await;
        ui.session().settings().set_project_trusted(true);
        mock.marketplace(&ui.agent_dir, false).install("todo", None).await.unwrap();
        let manager = ui.package_manager();
        manager.add_source_to_settings(SOURCE, false).unwrap();
        manager.add_source_to_settings(SOURCE, true).unwrap();
        let snapshot = backend(&ui, &mock).snapshot().await.unwrap();
        assert_eq!(ids(&snapshot), [format!("package:User:{SOURCE}"), format!("package:Project:{SOURCE}")]);
        for (row, scope) in snapshot.rows.iter().zip([SourceScope::User, SourceScope::Project]) {
            assert_eq!(row.source, format!("{SOURCE} ({scope:?})"));
        }
        // The project copy shadows the user copy, so only it provides the extension.
        let paths = |row: usize| snapshot.targets[&snapshot.rows[row].id].paths.clone();
        assert_eq!((paths(0), paths(1)), (Vec::<String>::new(), vec![NATIVE_PATH.to_string()]));
    })
    .await;
}
