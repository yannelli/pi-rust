use std::path::Path;

use serde_json::{Value, json};

use super::*;

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn moves_oauth_and_settings_api_keys_to_auth_json() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path();
    std::fs::write(agent.join("oauth.json"), r#"{"anthropic": {"access": "a", "refresh": "r", "expires": 1}}"#)
        .unwrap();
    let settings = json!({ "theme": "dark", "apiKeys": { "anthropic": "ignored", "openai": "sk-1", "bad": 3 } });
    std::fs::write(agent.join("settings.json"), format!("\u{feff}{settings}")).unwrap();

    let providers = migrate_auth_to_auth_json(agent);
    assert_eq!(providers, ["anthropic", "openai"]);
    let auth = read(&agent.join("auth.json"));
    assert_eq!(auth["anthropic"], json!({ "type": "oauth", "access": "a", "refresh": "r", "expires": 1 }));
    assert_eq!(auth["openai"], json!({ "type": "api_key", "key": "sk-1" }));
    assert!(auth.get("bad").is_none());
    assert!(!agent.join("oauth.json").exists());
    assert!(agent.join("oauth.json.migrated").exists());
    assert_eq!(read(&agent.join("settings.json")), json!({ "theme": "dark" }));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(agent.join("auth.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // An existing auth.json stops the migration.
    std::fs::write(agent.join("oauth.json"), r#"{"x": {}}"#).unwrap();
    assert!(migrate_auth_to_auth_json(agent).is_empty());
    assert!(agent.join("oauth.json").exists());
}

#[test]
fn nothing_to_migrate_writes_no_auth_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), r#"{"theme": "dark"}"#).unwrap();
    assert!(migrate_auth_to_auth_json(dir.path()).is_empty());
    assert!(!dir.path().join("auth.json").exists());
    assert_eq!(read(&dir.path().join("settings.json")), json!({ "theme": "dark" }));
}

#[test]
fn moves_root_sessions_into_their_cwd_directory() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path();
    let header = json!({ "type": "session", "id": "s1", "cwd": "/home/u/proj" });
    std::fs::write(agent.join("a.jsonl"), format!("{header}\n{{}}\n")).unwrap();
    std::fs::write(agent.join("not-session.jsonl"), "{\"type\":\"message\"}\n").unwrap();
    std::fs::write(agent.join("empty.jsonl"), "").unwrap();
    std::fs::write(agent.join("win.jsonl"), format!("{}\n", json!({ "type": "session", "cwd": "C:\\work\\x" })))
        .unwrap();

    migrate_sessions_from_agent_root(agent);
    assert!(agent.join("sessions/--home-u-proj--/a.jsonl").exists());
    assert!(!agent.join("a.jsonl").exists());
    assert!(agent.join("sessions/--C--work-x--/win.jsonl").exists());
    assert!(agent.join("not-session.jsonl").exists());
    assert!(agent.join("empty.jsonl").exists());

    // An existing target keeps the source in place.
    std::fs::write(agent.join("a.jsonl"), format!("{header}\n")).unwrap();
    migrate_sessions_from_agent_root(agent);
    assert!(agent.join("a.jsonl").exists());
}

#[test]
fn renames_commands_and_moves_managed_binaries() {
    let dir = tempfile::tempdir().unwrap();
    let (agent, cwd) = (dir.path().join("agent"), dir.path().join("proj"));
    std::fs::create_dir_all(agent.join("commands")).unwrap();
    std::fs::write(agent.join("commands/review.md"), "x").unwrap();
    std::fs::create_dir_all(project_config_dir(&cwd).join("commands")).unwrap();
    std::fs::create_dir_all(project_config_dir(&cwd).join("prompts")).unwrap();
    std::fs::create_dir_all(agent.join("tools")).unwrap();
    std::fs::create_dir_all(agent.join("bin")).unwrap();
    std::fs::write(agent.join("tools/fd"), "new fd").unwrap();
    std::fs::write(agent.join("tools/rg"), "old rg").unwrap();
    std::fs::write(agent.join("bin/rg"), "kept rg").unwrap();
    std::fs::write(agent.join("tools/.DS_Store"), "").unwrap();

    let result = run_migrations(&cwd, &agent);
    assert!(agent.join("prompts/review.md").exists());
    assert!(!agent.join("commands").exists());
    // Project prompts/ already exists, so commands/ stays.
    assert!(project_config_dir(&cwd).join("commands").exists());
    assert_eq!(std::fs::read_to_string(agent.join("bin/fd")).unwrap(), "new fd");
    assert_eq!(std::fs::read_to_string(agent.join("bin/rg")).unwrap(), "kept rg");
    assert!(!agent.join("tools/rg").exists());
    assert!(result.deprecation_warnings.is_empty(), "{:?}", result.deprecation_warnings);
}

#[test]
fn warns_about_hooks_and_custom_tools() {
    let dir = tempfile::tempdir().unwrap();
    let (agent, cwd) = (dir.path().join("agent"), dir.path().join("proj"));
    std::fs::create_dir_all(agent.join("hooks")).unwrap();
    std::fs::create_dir_all(project_config_dir(&cwd).join("tools")).unwrap();
    std::fs::write(project_config_dir(&cwd).join("tools/my-tool.ts"), "").unwrap();
    std::fs::write(project_config_dir(&cwd).join("tools/RG"), "").unwrap();

    let result = run_migrations(&cwd, &agent);
    assert_eq!(
        result.deprecation_warnings,
        [
            "Global hooks/ directory found. Hooks have been renamed to extensions.",
            "Project tools/ directory contains custom tools. Custom tools have been merged into extensions.",
        ]
    );
    let text = deprecation_warning_text(&result.deprecation_warnings);
    assert!(text.contains("Warning: Global hooks/ directory found."));
    assert!(text.contains("Migration guide: https://"));
    assert!(text.contains("Press any key to continue..."));
}

#[test]
fn rewrites_legacy_keybinding_names() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keybindings.json"), r#"{"cursorUp": "ctrl+p"}"#).unwrap();
    run_migrations(dir.path(), dir.path());
    let text = std::fs::read_to_string(dir.path().join("keybindings.json")).unwrap();
    assert!(text.ends_with("}\n"));
    assert_eq!(read(&dir.path().join("keybindings.json")), json!({ "tui.editor.cursorUp": "ctrl+p" }));
}
