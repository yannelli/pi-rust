use serde_json::json;

use super::*;
use crate::packages::{
    CommandOutput, ConfigEditor, ConfigWriteScope, PackageCommandOptions, ProjectOverrideState, ScopedResolvedPaths,
    run_config_command,
};
use crate::resources::{PathMetadata, ResolvedPaths, ResolvedResource, SourceOrigin, SourceScope};
use crate::settings::{InMemorySettingsStorage, SettingsOptions, SettingsStorage};

fn memory_settings(global: serde_json::Value) -> Arc<SettingsManager> {
    let storage = InMemorySettingsStorage::default();
    let text = global.to_string();
    storage.with_lock(crate::settings::SettingsScope::Global, &mut |_| Ok(Some(text.clone()))).unwrap();
    Arc::new(SettingsManager::from_storage(Arc::new(storage), SettingsOptions { project_trusted: true }))
}

fn package_extensions(root: &Path, source: &str, files: &[&str]) -> ResolvedPaths {
    let mut metadata = PathMetadata::new(source, SourceScope::User, SourceOrigin::Package);
    metadata.base_dir = Some(root.to_path_buf());
    metadata.package_root = Some(root.to_path_buf());
    let extensions = files
        .iter()
        .map(|file| ResolvedResource {
            path: root.join("extensions").join(file),
            enabled: true,
            metadata: metadata.clone(),
        })
        .collect();
    ResolvedPaths { extensions, ..ResolvedPaths::default() }
}

fn editor(
    fixture: &Fixture,
    settings: Arc<SettingsManager>,
    resolved: ResolvedPaths,
    scope: ConfigWriteScope,
) -> ConfigEditor {
    let scoped = ScopedResolvedPaths { global: resolved.clone(), project: resolved };
    ConfigEditor::new(&scoped, settings, &fixture.cwd, &fixture.agent_dir, &fixture.root(), scope)
}

#[test]
fn project_mode_cycles_package_overrides() {
    let fixture = Fixture::new();
    let settings = memory_settings(json!({"packages": ["npm:pi-tools"]}));
    let resolved = package_extensions(&fixture.root().join("pkg"), "npm:pi-tools", &["bar.ts"]);
    let mut editor = editor(&fixture, settings.clone(), resolved, ConfigWriteScope::Project);
    assert_eq!(editor.groups()[0].label, "npm:pi-tools (user)");
    let project = || serde_json::to_value(settings.project_settings().packages.unwrap_or_default()).unwrap();
    assert_eq!(editor.toggle(0, 0, 0), Some(false));
    assert_eq!(project(), json!([{"source": "npm:pi-tools", "autoload": false, "extensions": ["-extensions/bar.ts"]}]));
    let item = editor.groups()[0].subgroups[0].items[0].clone();
    assert_eq!((editor.override_state(&item), editor.checkbox(&item)), (ProjectOverrideState::Unload, "[-]"));
    assert_eq!(editor.toggle(0, 0, 0), Some(true));
    assert_eq!(project(), json!([{"source": "npm:pi-tools", "autoload": false, "extensions": ["+extensions/bar.ts"]}]));
    assert_eq!(editor.toggle(0, 0, 0), Some(true));
    assert_eq!(project(), json!([]));
    assert_eq!(editor.item_suffix(&editor.groups()[0].subgroups[0].items[0]), "inherited global");
}

#[test]
fn global_mode_toggles_package_resources_with_filters() {
    let fixture = Fixture::new();
    let settings = memory_settings(json!({"packages": ["npm:pi-tools"]}));
    let resolved = package_extensions(&fixture.root().join("pkg"), "npm:pi-tools", &["a.ts", "b.ts"]);
    let mut editor = editor(&fixture, settings.clone(), resolved, ConfigWriteScope::Global);
    assert_eq!(editor.toggle(0, 0, 1), Some(false));
    let global = serde_json::to_value(settings.global_settings().packages.unwrap()).unwrap();
    assert_eq!(global, json!([{"source": "npm:pi-tools", "extensions": ["-extensions/b.ts"]}]));
    assert!(!editor.groups()[0].subgroups[0].items[1].enabled);
}

#[tokio::test]
async fn builtins_toggle_globally_and_cycle_per_project() {
    let fixture = Fixture::new();
    let resolve = |settings: Arc<SettingsManager>| {
        let mut options = PackageManagerOptions::new(&fixture.cwd, &fixture.agent_dir, settings);
        options.home = fixture.root();
        options.runner = MockRunner::failing();
        options.builtin_extensions = vec!["llama.cpp".into(), "mcp".into()];
        PackageManager::new(options)
    };
    let settings = memory_settings(json!({}));
    let resolved = resolve(settings.clone()).resolve(None).await.unwrap();
    let mut global = editor(&fixture, settings.clone(), resolved, ConfigWriteScope::Global);
    let group = global.groups().iter().position(|group| group.label == "Built-in").unwrap();
    assert_eq!(global.groups()[group].subgroups[0].items[0].display_name, "llama.cpp");
    global.toggle(group, 0, 0);
    assert_eq!(settings.global_settings().extensions, Some(vec!["-builtin:llama.cpp".to_string()]));
    global.toggle(group, 0, 0);
    assert_eq!(settings.global_settings().extensions, Some(vec!["+builtin:llama.cpp".to_string()]));

    let settings = memory_settings(json!({"extensions": ["-builtin:mcp"]}));
    let mut builtins = PackageManagerOptions::new(&fixture.cwd, &fixture.agent_dir, settings.clone());
    builtins.home = fixture.root();
    builtins.runner = MockRunner::failing();
    builtins.builtin_extensions = vec!["mcp".into()];
    let resolved = PackageManager::new(builtins).resolve(None).await.unwrap();
    let mut project = editor(&fixture, settings.clone(), resolved, ConfigWriteScope::Project);
    let expected = [vec!["+builtin:mcp".to_string()], vec!["-builtin:mcp".to_string()], vec![]];
    for entries in expected {
        project.toggle(0, 0, 0);
        assert_eq!(settings.project_settings().extensions.unwrap_or_default(), entries);
    }
}

#[tokio::test]
async fn config_command_prints_resources_without_a_selector() {
    let fixture = Fixture::new();
    write(&fixture.agent_dir.join("skills/review/SKILL.md"), &skill("review"));
    write(&fixture.agent_dir.join("settings.json"), &json!({"skills": ["-skills/review/SKILL.md"]}).to_string());
    let mut options = PackageCommandOptions::new(fixture.cwd.clone(), fixture.agent_dir.clone());
    options.home = fixture.root();
    options.runner = MockRunner::failing();
    let (stdout, out) = CommandOutput::capture();
    let (stderr, err) = CommandOutput::capture();
    (options.stdout, options.stderr) = (stdout, stderr);
    let code = run_config_command(&["config".to_string()], &options, None).await.unwrap();
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err.lock().unwrap()));
    let expected = format!(
        "Global Resources\n~/.pi/agent/settings.json\n\nUser ({}/)\n  Skills\n    [ ] review\n",
        posix(&fixture.agent_dir)
    );
    assert_eq!(text, expected.replace(&posix(&fixture.root()), "~"));

    write(&fixture.cwd.join(".pi/settings.json"), "{}");
    let code = run_config_command(&["config".to_string(), "-l".to_string()], &options, None).await.unwrap();
    assert_eq!(code, 1);
    assert!(
        String::from_utf8_lossy(&err.lock().unwrap())
            .contains("Project is not trusted. Use --approve to modify local resource config.")
    );
    let unknown = run_config_command(&["config".to_string(), "--bad".to_string()], &options, None).await;
    assert_eq!(unknown, Some(1));
    assert!(run_config_command(&["list".to_string()], &options, None).await.is_none());
}
