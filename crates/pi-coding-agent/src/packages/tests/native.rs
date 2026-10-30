use serde_json::json;

use super::*;
use crate::packages::{ConfigEditor, ConfigWriteScope, ScopedResolvedPaths};
use crate::resources::{ResourceLoader, ResourceLoaderOptions};
use crate::settings::{InMemorySettingsStorage, SettingsStorage};

const MANIFEST: &str = r#"{"name": "@x/tools", "pi": {"extensions": ["main.ts"], "native": [
    {"command": "bin/ext", "args": ["todo"]}, {"command": "bin/ext", "args": ["memo"]}]}}"#;

fn memory_settings(global: serde_json::Value) -> Arc<SettingsManager> {
    let storage = InMemorySettingsStorage::default();
    let text = global.to_string();
    storage.with_lock(crate::settings::SettingsScope::Global, &mut |_| Ok(Some(text.clone()))).unwrap();
    Arc::new(SettingsManager::from_storage(Arc::new(storage), SettingsOptions { project_trusted: true }))
}

#[tokio::test]
async fn native_entries_resolve_toggle_and_load_with_their_package() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("pkg");
    write(&pkg.join("package.json"), MANIFEST);
    write(&pkg.join("main.ts"), "export default () => {};");
    fixture.set_global(json!({"packages": ["../pkg"]}));
    let resolved = fixture.manager(MockRunner::failing()).resolve_package_resources(None).await.unwrap();
    let paths: Vec<String> = resolved.extensions.iter().map(|resource| posix(&resource.path)).collect();
    assert_eq!(paths[1..], ["native:@x/tools/todo", "native:@x/tools/memo"]);
    assert!(paths[0].ends_with("pkg/main.ts"));

    let settings = memory_settings(json!({"packages": ["../pkg"]}));
    let scoped = ScopedResolvedPaths { global: resolved.clone(), project: resolved.clone() };
    let mut editor = ConfigEditor::new(
        &scoped,
        settings.clone(),
        &fixture.cwd,
        &fixture.agent_dir,
        &fixture.root(),
        ConfigWriteScope::Global,
    );
    let items = &editor.groups()[0].subgroups[0].items;
    let index = items.iter().position(|item| item.display_name == "native:@x/tools/memo").unwrap();
    assert_eq!(editor.toggle(0, 0, index), Some(false));
    let global = serde_json::to_value(settings.global_settings().packages.unwrap()).unwrap();
    assert_eq!(global, json!([{"source": "../pkg", "extensions": ["-native:@x/tools/memo"]}]));

    fixture.set_global(json!({"packages": [{"source": "../pkg", "extensions": ["-native:@x/tools/memo"]}]}));
    let manager = fixture.manager(MockRunner::failing());
    let resolved = manager.resolve_package_resources(None).await.unwrap();
    let state: Vec<(String, bool)> = resolved.extensions.iter().map(|r| (posix(&r.path), r.enabled)).collect();
    assert_eq!(state[1..], [("native:@x/tools/todo".into(), true), ("native:@x/tools/memo".into(), false)]);

    let mut options = ResourceLoaderOptions::new(&fixture.cwd, &fixture.agent_dir);
    options.home = fixture.root();
    options.package_resources = resolved;
    let mut loader = ResourceLoader::new(options);
    loader.reload();
    let entries = crate::extensions::load_entries(&loader.resources().extensions);
    let native: Vec<(&str, &serde_json::Value)> =
        entries.iter().filter_map(|entry| Some((entry.path.as_str(), entry.native.as_ref()?))).collect();
    assert_eq!(native.len(), 1);
    assert_eq!(native[0].0, "native:@x/tools/todo");
    let program = crate::packages::manifest::native_program(&pkg, "bin/ext");
    assert_eq!(native[0].1["command"], program.display().to_string());
    assert_eq!(native[0].1["args"], json!(["todo"]));
}
