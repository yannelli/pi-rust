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

#[test]
fn explicit_extension_state_is_idempotent_and_rejects_ambiguous_names() {
    let fixture = Fixture::new();
    let settings = memory_settings(json!({"packages": ["npm:pi-tools", "npm:other"]}));
    let mut resolved = package_extensions(&fixture.root().join("one"), "npm:pi-tools", &["a.ts"]);
    resolved.extensions.extend(package_extensions(&fixture.root().join("two"), "npm:other", &["a.ts"]).extensions);
    let target = resolved.extensions[0].path.display().to_string();
    let mut editor = editor(&fixture, settings.clone(), resolved, ConfigWriteScope::Project);
    assert!(editor.set_extension_enabled("a.ts", true).unwrap_err().contains("Ambiguous"));
    assert!(settings.project_settings().packages.is_none());
    for _ in 0..2 {
        editor.set_extension_enabled(&target, true).unwrap();
    }
    let project = serde_json::to_value(settings.project_settings().packages.unwrap()).unwrap();
    assert_eq!(project, json!([{"source": "npm:pi-tools", "autoload": false, "extensions": ["+extensions/a.ts"]}]));
    editor.set_extension_enabled(&target, false).unwrap();
    let project = serde_json::to_value(settings.project_settings().packages.unwrap()).unwrap();
    assert_eq!(project, json!([{"source": "npm:pi-tools", "autoload": false, "extensions": ["-extensions/a.ts"]}]));
}

#[tokio::test]
async fn extensions_cli_lists_and_sets_builtin_state_without_removing_it() {
    let fixture = Fixture::new();
    let mut options = PackageCommandOptions::new(fixture.cwd.clone(), fixture.agent_dir.clone());
    options.home = fixture.root();
    options.runner = MockRunner::failing();
    options.builtin_extensions = vec!["mcp".into(), "llama.cpp".into()];
    let (stdout, out) = CommandOutput::capture();
    let (stderr, _) = CommandOutput::capture();
    (options.stdout, options.stderr) = (stdout, stderr);
    let args = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for _ in 0..2 {
        assert_eq!(run_config_command(&args(&["extensions", "disable", "builtin:mcp"]), &options, None).await, Some(0));
    }
    assert_eq!(run_config_command(&args(&["extensions", "list"]), &options, None).await, Some(0));
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    assert!(text.contains("[ ] builtin:mcp") && text.contains("[x] builtin:llama.cpp"), "{text}");
    assert_eq!(run_config_command(&args(&["extensions", "enable", "mcp"]), &options, None).await, Some(0));
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.agent_dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved["extensions"], json!(["+builtin:mcp"]));
    assert_eq!(run_config_command(&args(&["extensions", "enable"]), &options, None).await, Some(1));
    assert_eq!(run_config_command(&args(&["extensions", "enable", "missing"]), &options, None).await, Some(1));
    write(&fixture.cwd.join(".pi/settings.json"), "{}");
    assert_eq!(run_config_command(&args(&["extensions", "disable", "mcp", "-l"]), &options, None).await, Some(1));
    assert_eq!(
        run_config_command(&args(&["extensions", "disable", "mcp", "-l", "--approve"]), &options, None).await,
        Some(0)
    );
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

#[tokio::test]
async fn extensions_list_reports_an_invalid_marketplace_entry_and_loads_the_rest() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("local-tools");
    write(&pkg.join("extensions/index.ts"), "export default function () {}\n");
    fixture.set_global(json!({"packages": ["marketplace:Todo", pkg.display().to_string()]}));
    let mut options = PackageCommandOptions::new(fixture.cwd.clone(), fixture.agent_dir.clone());
    options.home = fixture.root();
    options.runner = MockRunner::failing();
    let (stdout, out) = CommandOutput::capture();
    let (stderr, err) = CommandOutput::capture();
    (options.stdout, options.stderr) = (stdout, stderr);
    let code = run_config_command(&["extensions".to_string(), "list".to_string()], &options, None).await;
    let (text, errors) = (
        String::from_utf8_lossy(&out.lock().unwrap()).into_owned(),
        String::from_utf8_lossy(&err.lock().unwrap()).into_owned(),
    );
    assert_eq!(code, Some(0), "{errors}");
    assert!(text.contains("local-tools/extensions/index.ts"), "{text}");
    assert_eq!(errors.matches("Warning: Extension package").count(), 1, "{errors}");
    assert!(errors.contains("Extension package \"marketplace:Todo\": failed to install"), "{errors}");
}
