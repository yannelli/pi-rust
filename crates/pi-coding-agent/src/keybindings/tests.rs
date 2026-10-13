use super::*;
use serde_json::json;

fn keys(definitions: &KeybindingDefinitions, id: &str) -> Vec<String> {
    definitions.get(id).map(|definition| definition.default_keys.clone()).unwrap_or_default()
}

#[test]
fn windows_defaults_follow_platform_and_wsl() {
    let none = |_: &str| None;
    assert!(use_windows_keybindings_for("win32", none));
    assert!(use_windows_keybindings_for("linux", |name| (name == "WSL_DISTRO_NAME").then(|| "Ubuntu".into())));
    assert!(use_windows_keybindings_for("linux", |name| (name == "WSL_INTEROP").then(|| "/run/WSL/1".into())));
    assert!(!use_windows_keybindings_for("linux", |name| (name == "WT_SESSION").then(|| "session".into())));
    assert!(!use_windows_keybindings_for("linux", none));
    assert!(!use_windows_keybindings_for("darwin", none));
}

#[test]
fn platform_defaults_apply_consistently() {
    for (platform, windows) in [("linux", false), ("linux", true), ("win32", true), ("darwin", false)] {
        let definitions = keybindings_for(platform, windows);
        let pick = |a: &str, b: &str| vec![if windows { a } else { b }.to_string()];
        assert_eq!(keys(&definitions, "app.clipboard.pasteImage"), pick("alt+v", "ctrl+v"));
        assert_eq!(keys(&definitions, "tui.altScreen.search"), pick("ctrl+f", "ctrl+shift+f"));
        assert_eq!(keys(&definitions, "app.message.followUp"), pick("ctrl+q", "alt+enter"));
        assert_eq!(keys(&definitions, "app.model.cycleBackward"), pick("alt+p", "shift+ctrl+p"));
        assert_eq!(keys(&definitions, "app.message.dequeue"), pick("alt+q", "alt+up"));
        let undo = if platform == "win32" { vec!["ctrl+z".to_string()] } else { pick("alt+z", "ctrl+-") };
        assert_eq!(keys(&definitions, "tui.editor.undo"), undo);
        let previous: Vec<&str> = if windows { vec!["ctrl+up"] } else { vec!["ctrl+shift+up", "ctrl+up"] };
        assert_eq!(keys(&definitions, "tui.altScreen.previousPrompt"), previous);
    }
    assert!(keys(&keybindings_for("win32", true), "app.suspend").is_empty());
    assert_eq!(keys(&keybindings_for("darwin", false), "app.tree.foldOrUp"), ["alt+left", "ctrl+left"]);
    assert_eq!(keys(&keybindings_for("linux", false), "app.tree.foldOrUp"), ["ctrl+left", "alt+left"]);
}

#[test]
fn app_ids_follow_the_tui_ids() {
    let definitions = keybindings_for("linux", false);
    let ids: Vec<&str> = definitions.ids().collect();
    let first_app = ids.iter().position(|id| id.starts_with("app.")).unwrap();
    assert_eq!(ids[first_app], "app.interrupt");
    assert!(ids[first_app..].iter().all(|id| id.starts_with("app.")));
    assert_eq!(ids.last(), Some(&"app.tree.filter.cycleBackward"));
}

#[test]
fn migration_renames_legacy_names_and_prefers_namespaced_values() {
    let raw = json!({"cursorUp": ["up", "ctrl+p"], "expandTools": "ctrl+x"});
    let (config, migrated) = migrate_keybindings_config(raw.as_object().unwrap());
    assert!(migrated);
    assert_eq!(Value::Object(config), json!({"tui.editor.cursorUp": ["up", "ctrl+p"], "app.tools.expand": "ctrl+x"}));

    let raw = json!({"expandTools": "ctrl+x", "app.tools.expand": "ctrl+y"});
    let (config, migrated) = migrate_keybindings_config(raw.as_object().unwrap());
    assert!(migrated);
    assert_eq!(Value::Object(config), json!({"app.tools.expand": "ctrl+y"}));
}

#[test]
fn migration_orders_known_ids_first_and_sorts_extras() {
    let raw = json!({"zzz": "a", "app.exit": "ctrl+q", "aaa": "b", "tui.select.up": "k"});
    let (config, migrated) = migrate_keybindings_config(raw.as_object().unwrap());
    assert!(!migrated);
    let order: Vec<&str> = config.keys().map(String::as_str).collect();
    assert_eq!(order, ["tui.select.up", "app.exit", "aaa", "zzz"]);
}

#[test]
fn file_migration_rewrites_only_when_needed() {
    let dir = tempfile::tempdir().unwrap();
    let path = keybindings_path(dir.path());
    std::fs::write(&path, "{\n  \"cursorUp\": [\"up\", \"ctrl+p\"],\n  \"expandTools\": \"ctrl+x\"\n}\n").unwrap();
    migrate_keybindings_config_file(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        "{\n  \"tui.editor.cursorUp\": [\n    \"up\",\n    \"ctrl+p\"\n  ],\n  \"app.tools.expand\": \"ctrl+x\"\n}\n"
    );
}

#[test]
fn legacy_names_load_in_memory_before_the_file_is_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(keybindings_path(dir.path()), r#"{"selectConfirm": "enter", "interrupt": "ctrl+x"}"#).unwrap();
    let manager = KeybindingsManager::create(dir.path());
    let user: Vec<(String, KeybindingValue)> = manager.get_user_bindings().into_iter().collect();
    assert_eq!(
        user,
        [
            ("tui.select.confirm".to_string(), KeybindingValue::One("enter".into())),
            ("app.interrupt".to_string(), KeybindingValue::One("ctrl+x".into())),
        ]
    );
    let effective = manager.get_effective_config();
    assert_eq!(effective.get("tui.select.confirm"), Some(&KeybindingValue::One("enter".into())));
    assert_eq!(effective.get("app.interrupt"), Some(&KeybindingValue::One("ctrl+x".into())));
}

#[test]
fn invalid_values_and_schema_are_ignored_and_reload_rereads() {
    let dir = tempfile::tempdir().unwrap();
    let path = keybindings_path(dir.path());
    std::fs::write(
        &path,
        "\u{feff}{\"$schema\": \"x\", \"app.exit\": \"ctrl+ctrl+d\", \"app.clear\": [\"ctrl+k\", 3], \"app.model.select\": []}",
    )
    .unwrap();
    let mut manager = KeybindingsManager::create(dir.path());
    assert_eq!(manager.get_keys("app.exit"), ["ctrl+d"]);
    assert_eq!(manager.get_keys("app.clear"), ["ctrl+c"]);
    assert!(manager.get_keys("app.model.select").is_empty());
    std::fs::write(&path, r#"{"app.exit": "ctrl+q"}"#).unwrap();
    manager.reload();
    assert_eq!(manager.get_keys("app.exit"), ["ctrl+q"]);
    assert_eq!(manager.get_keys("app.model.select"), ["ctrl+l"]);
}

#[test]
fn key_ids_follow_the_schema() {
    for valid in ["a", "ctrl+c", "shift+ctrl+p", "ctrl++", "alt+pageUp", "ctrl+shift+alt+super+f12", "`", "?"] {
        assert!(is_valid_key_id(valid), "{valid}");
    }
    for invalid in ["", "ctrl+", "A", "ctrl+ctrl+c", "ctrl+shift+ctrl+x", "hyper+a", "ab", "PageUp"] {
        assert!(!is_valid_key_id(invalid), "{invalid}");
    }
}
