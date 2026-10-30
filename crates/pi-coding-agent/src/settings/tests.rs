use std::path::Path;

use pi_agent::QueueMode;
use pi_ai::{Model, ThinkingLevel, Transport};
use serde_json::{Value, json};

use super::schema::{CacheWarmingMode, TuiMode};
use super::*;

fn write(path: &Path, value: Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct Dirs {
    _root: tempfile::TempDir,
    cwd: std::path::PathBuf,
    agent: std::path::PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        let agent = root.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        Self { _root: root, cwd, agent }
    }

    fn global(&self) -> std::path::PathBuf {
        self.agent.join("settings.json")
    }

    fn project(&self) -> std::path::PathBuf {
        self.cwd.join(".pi/settings.json")
    }

    fn manager(&self, trusted: bool) -> SettingsManager {
        SettingsManager::create(&self.cwd, &self.agent, SettingsOptions { project_trusted: trusted })
    }
}

#[test]
fn status_line_editor_persists_global_settings_without_project_commands() {
    let dirs = Dirs::new();
    write(&dirs.global(), json!({"theme": "dark", "statusLine": {"command": "safe.sh", "custom": true}}));
    write(&dirs.project(), json!({"statusLine": {"command": "project.sh", "theme": "mono"}}));
    let settings = dirs.manager(true);
    let mut value = settings.global_status_line_value().unwrap();
    value["layout"] = json!("compact");
    value["segments"] = json!(["model", "contextHeadroom"]);
    settings.set_status_line(value.clone()).unwrap();
    assert_eq!(read(&dirs.global())["statusLine"], value);
    assert_eq!(read(&dirs.global())["theme"], "dark");
    assert_eq!(settings.status_line().unwrap().command.as_deref(), Some("safe.sh"));
    assert_eq!(read(&dirs.project())["statusLine"]["command"], "project.sh");
    assert_eq!(dirs.manager(true).global_status_line_value(), Some(value));
}

#[test]
fn status_line_save_errors_do_not_change_memory_or_corrupt_files() {
    let dirs = Dirs::new();
    write(&dirs.global(), json!({"statusLine": {"preset": "minimal"}}));
    let settings = dirs.manager(true);
    std::fs::write(dirs.global(), "broken JSON").unwrap();
    assert!(settings.set_status_line(json!({"preset": "compact"})).is_err());
    assert_eq!(settings.global_status_line_value(), Some(json!({"preset": "minimal"})));
    assert_eq!(std::fs::read_to_string(dirs.global()).unwrap(), "broken JSON");
    let failed = dirs.manager(true);
    assert!(failed.set_status_line(json!({"preset": "compact"})).is_err());
    assert!(failed.global_status_line_value().is_none());
}

#[test]
fn writes_keep_unknown_keys_and_external_edits() {
    let dirs = Dirs::new();
    write(&dirs.global(), json!({"theme": "dark", "enabledModels": ["a/*"], "customKey": {"x": 1}}));
    let settings = dirs.manager(true);
    let mut external = read(&dirs.global());
    external["packages"] = json!(["npm:external"]);
    external["theme"] = json!("light");
    write(&dirs.global(), external);
    settings.set_default_thinking_level(ThinkingLevel::Max);
    settings.set_theme("solarized");
    let saved = read(&dirs.global());
    assert_eq!(saved["enabledModels"], json!(["a/*"]));
    assert_eq!(saved["customKey"]["x"], 1);
    assert_eq!(saved["packages"], json!(["npm:external"]));
    assert_eq!(saved["theme"], "solarized");
    assert_eq!(saved["defaultThinkingLevel"], "max");
    assert_eq!(settings.default_thinking_level(), Some(ThinkingLevel::Max));
}

#[test]
fn project_settings_merge_over_global_and_respect_trust() {
    let dirs = Dirs::new();
    write(
        &dirs.global(),
        json!({"defaultProvider": "openai", "compaction": {"enabled": false, "reserveTokens": 1000},
        "sessionDir": "~/global-sessions", "defaultTools": ["read", "bash"], "skills": ["global-skill"]}),
    );
    write(
        &dirs.project(),
        json!({"defaultModel": "m", "compaction": {"keepRecentTokens": 5},
        "sessionDir": "./project-sessions", "defaultTools": ["+ls", "-bash"], "skills": ["project-skill"]}),
    );
    let settings = dirs.manager(true);
    assert_eq!(
        (settings.default_provider().as_deref(), settings.default_model().as_deref()),
        (Some("openai"), Some("m"))
    );
    let compaction = settings.compaction_settings(None);
    assert_eq!((compaction.enabled, compaction.reserve_tokens, compaction.keep_recent_tokens), (false, 1000, 5));
    assert_eq!(settings.session_dir().unwrap(), Path::new("./project-sessions"));
    assert_eq!(settings.default_tools().unwrap(), ["read", "ls"]);
    assert_eq!(settings.merged_resource_paths(ResourceKind::Skills), ["project-skill"]);
    let combined = settings.combined_resource_paths(ResourceKind::Skills);
    assert_eq!(
        combined,
        [(SettingsScope::Project, "project-skill".into()), (SettingsScope::Global, "global-skill".into())]
    );

    let untrusted = dirs.manager(false);
    assert_eq!(untrusted.default_model(), None);
    assert!(untrusted.set_project_value("theme", Some(json!("x"))).is_err());
    untrusted.set_project_trusted(true);
    assert_eq!(untrusted.default_model().as_deref(), Some("m"));
    let home = untrusted.global_settings();
    assert_eq!(home.session_dir.as_deref(), Some("~/global-sessions"));
}

#[test]
fn reading_never_creates_project_dirs_and_project_writes_do() {
    let dirs = Dirs::new();
    let settings = dirs.manager(true);
    assert!(!dirs.cwd.join(".pi").exists());
    settings.set_project_resource_paths(ResourceKind::Extensions, &["./ext".to_string()]).unwrap();
    assert_eq!(read(&dirs.project())["extensions"], json!(["./ext"]));
    assert!(!dirs.global().exists());
}

#[test]
fn invalid_files_report_diagnostics_and_are_never_overwritten() {
    let dirs = Dirs::new();
    std::fs::write(dirs.global(), "{").unwrap();
    let settings = dirs.manager(true);
    let errors = settings.drain_errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].diagnostic().starts_with(&format!("Invalid settings file {}:", dirs.global().display())));
    assert!(settings.drain_errors().is_empty());
    settings.set_theme("dark");
    assert_eq!(std::fs::read_to_string(dirs.global()).unwrap(), "{");

    write(&dirs.global(), json!({"retry": {"maxRetries": "many"}, "tuiMode": "sideways", "theme": "ok"}));
    settings.reload();
    let messages: Vec<String> = settings.drain_errors().iter().map(SettingsError::diagnostic).collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages.iter().any(|message| message.contains("\"tuiMode\"")));
    assert_eq!(settings.theme().as_deref(), Some("ok"));
    assert_eq!(settings.tui_mode(), TuiMode::Fullscreen);
    assert_eq!(settings.retry_settings().max_retries, 3);
}

#[test]
fn migrations_and_defaults_follow_upstream() {
    let settings = SettingsManager::in_memory(json!({"queueMode": "all", "websockets": true,
        "skills": {"enableSkillCommands": false, "customDirectories": ["~/skills"]}, "retry": {"maxDelayMs": 5000}}));
    assert_eq!(settings.steering_mode(), QueueMode::All);
    assert_eq!(settings.follow_up_mode(), QueueMode::OneAtATime);
    assert_eq!(settings.transport(), Transport::Websocket);
    assert!(!settings.enable_skill_commands());
    assert_eq!(settings.merged_resource_paths(ResourceKind::Skills), ["~/skills"]);
    assert_eq!(settings.provider_retry_settings().max_retry_delay_ms, 5000);

    let defaults = SettingsManager::in_memory(json!({}));
    let retry = defaults.retry_settings();
    assert_eq!(
        (retry.enabled, retry.max_retries, retry.base_delay_ms, retry.max_agent_delay_ms),
        (true, 3, 2000, 60_000)
    );
    assert_eq!(defaults.transport(), Transport::Auto);
    assert_eq!(defaults.http_idle_timeout_ms(), Ok(300_000));
    assert_eq!(defaults.default_project_trust(), "ask");
    assert_eq!(defaults.quiet_startup(), QuietStartup::Off);
    assert_eq!((defaults.output_pad(), defaults.autocomplete_max_visible(), defaults.image_width_cells()), (1, 5, 60));
    assert!(defaults.default_tools().is_none());
    assert_eq!(DEFAULT_THINKING_LEVEL, ThinkingLevel::Medium);

    let custom = SettingsManager::in_memory(json!({"retry": {"maxAgentDelayMs": 5, "provider": {"timeoutMs": 10}},
        "httpIdleTimeoutMs": "disabled", "websocketConnectTimeoutMs": -1, "quietStartup": "header",
        "defaultProjectTrust": "bogus", "outputPad": 0, "fullscreenWheelScrollLines": 500}));
    assert_eq!(custom.retry_settings().max_agent_delay_ms, 5);
    assert_eq!(custom.provider_retry_settings().timeout_ms, Some(10));
    assert_eq!(custom.http_idle_timeout_ms(), Ok(0));
    assert!(custom.websocket_connect_timeout_ms().is_err());
    assert_eq!(custom.quiet_startup(), QuietStartup::Header);
    assert_eq!(custom.default_project_trust(), "ask");
    assert_eq!((custom.output_pad(), custom.fullscreen_wheel_scroll_lines()), (0, Some(100)));
}

#[test]
fn prompt_history_settings_default_to_on_with_1000_entries() {
    let defaults = SettingsManager::in_memory(json!({}));
    assert_eq!((defaults.prompt_history_enabled(), defaults.prompt_history_max_entries()), (true, 1000));
    let custom = SettingsManager::in_memory(json!({"promptHistory": {"enabled": false, "maxEntries": 50}}));
    assert_eq!((custom.prompt_history_enabled(), custom.prompt_history_max_entries()), (false, 50));
    let invalid = SettingsManager::in_memory(json!({"promptHistory": {"maxEntries": -3}}));
    assert_eq!(invalid.prompt_history_max_entries(), 1000);
}

#[test]
fn global_only_keys_ignore_project_values() {
    let dirs = Dirs::new();
    write(&dirs.global(), json!({"cacheWarming": "idle", "defaultProjectTrust": "always"}));
    write(
        &dirs.project(),
        json!({"cacheWarming": "off", "defaultProjectTrust": "never", "deviceId": "project-id",
        "httpProxy": "http://project"}),
    );
    let settings = dirs.manager(true);
    assert_eq!(settings.cache_warming_mode(), CacheWarmingMode::Idle);
    assert_eq!(settings.default_project_trust(), "always");
    assert_eq!(settings.http_proxy(), None);
    let id = settings.get_or_create_device_id();
    assert_ne!(id, "project-id");
    assert_eq!(id.len(), 36);
    assert_eq!(dirs.manager(true).get_or_create_device_id(), id);
}

#[test]
fn status_line_command_comes_only_from_global_settings() {
    let dirs = Dirs::new();
    write(&dirs.global(), json!({"statusLine": {"type": "command", "command": "global.sh", "padding": 1}}));
    write(&dirs.project(), json!({"statusLine": {"command": "project.sh", "padding": 2}}));
    let status_line = dirs.manager(true).status_line().expect("status line");
    assert_eq!((status_line.kind.as_deref(), status_line.command.as_deref()), (Some("command"), Some("global.sh")));
    assert_eq!(status_line.padding, Some(2.0));

    let dirs = Dirs::new();
    write(&dirs.project(), json!({"statusLine": {"type": "command", "command": "project.sh"}}));
    assert_eq!(dirs.manager(true).status_line().expect("status line").command, None);
    assert_eq!(SettingsManager::in_memory(json!({})).status_line(), None);
}

#[test]
fn iterm2_features_are_off_unless_turned_on() {
    let features =
        |value: serde_json::Value| SettingsManager::in_memory(json!({"terminal": {"iterm2": value}})).iterm2_features();
    assert!(!SettingsManager::in_memory(json!({})).iterm2_features().any());
    assert!(!features(json!(false)).any());
    let all = features(json!(true));
    assert!(all.tab_color && all.badge && all.user_vars && all.attention && all.cwd);
    let some = features(json!({"badge": false, "cwd": false}));
    assert!(some.tab_color && !some.badge && some.user_vars && some.attention && !some.cwd);
    assert!(!features(json!("yes")).any());

    let dirs = Dirs::new();
    let settings = dirs.manager(true);
    assert!(settings.enable_iterm2());
    assert!(settings.iterm2_features().any());
    assert!(!settings.enable_iterm2());
    let disabled = SettingsManager::in_memory(json!({"terminal": {"iterm2": false}}));
    assert!(disabled.enable_iterm2());
    assert!(disabled.iterm2_features().any());
    let custom = SettingsManager::in_memory(json!({"terminal": {"iterm2": {"badge": false}}}));
    assert!(!custom.enable_iterm2());
    assert!(!custom.iterm2_features().badge);
}

#[test]
fn model_scoped_values_and_nested_writes() {
    let dirs = Dirs::new();
    write(
        &dirs.global(),
        json!({"compaction": {"reserveTokens": 100, "modelOverrides": {"openrouter/a/b": {"reserveTokens": 7}}},
        "modelThinkingLevels": {"openai/gpt": "high"}, "shellPath": "~/bin/bash", "shellCommandPrefix": "shopt -s expand_aliases"}),
    );
    let settings = dirs.manager(true);
    let model = Model { provider: "openrouter".into(), id: "a/b".into(), ..Model::default() };
    assert_eq!(settings.compaction_settings(Some(&model)).reserve_tokens, 7);
    assert_eq!(settings.compaction_settings(None).reserve_tokens, 100);
    assert_eq!(settings.model_thinking_level("openai", "gpt"), Some(ThinkingLevel::High));
    assert!(settings.shell_path().unwrap().ends_with("bin/bash") && !settings.shell_path().unwrap().starts_with("~"));
    settings.set_compaction_enabled(false);
    settings.set_model_thinking_level("anthropic", "claude", ThinkingLevel::Low);
    settings.remove_model_thinking_level("openai", "gpt");
    settings.set_enabled_models(Some(&["anthropic/*".to_string()]));
    let saved = read(&dirs.global());
    assert_eq!(
        saved["compaction"],
        json!({"reserveTokens": 100, "modelOverrides": {"openrouter/a/b": {"reserveTokens": 7}}, "enabled": false})
    );
    assert_eq!(saved["modelThinkingLevels"], json!({"anthropic/claude": "low"}));
    assert_eq!(saved["shellCommandPrefix"], "shopt -s expand_aliases");
    assert_eq!(saved["enabledModels"], json!(["anthropic/*"]));
}

#[test]
fn retry_setters_persist_and_keep_sibling_keys() {
    let dirs = Dirs::new();
    write(&dirs.global(), json!({"retry": {"enabled": false, "custom": 1, "provider": {"extra": "x"}}}));
    let settings = dirs.manager(true);
    settings.set_retry_max_retries(5);
    settings.set_retry_base_delay_ms(500);
    settings.set_retry_max_agent_delay_ms(9_000);
    settings.set_provider_timeout_ms(Some(30_000));
    settings.set_provider_max_retries(Some(2));
    settings.set_provider_max_retry_delay_ms(4_000);
    let expected = RetryConfig { enabled: false, max_retries: 5, base_delay_ms: 500, max_agent_delay_ms: 9_000 };
    let provider = ProviderRetryConfig { timeout_ms: Some(30_000), max_retries: Some(2), max_retry_delay_ms: 4_000 };
    assert_eq!(settings.retry_settings(), expected);
    assert_eq!(settings.provider_retry_settings(), provider);
    let file = read(&dirs.global());
    assert_eq!(file["retry"]["custom"], 1);
    assert_eq!(file["retry"]["provider"]["extra"], "x");
    assert_eq!(file["retry"]["provider"]["timeoutMs"], 30_000);
    let reloaded = dirs.manager(true);
    assert_eq!(reloaded.retry_settings(), expected);
    assert_eq!(reloaded.provider_retry_settings(), provider);
}

#[test]
fn clearing_provider_retry_values_removes_keys_and_empty_objects() {
    let dirs = Dirs::new();
    let settings = dirs.manager(true);
    settings.set_provider_timeout_ms(Some(1_000));
    settings.set_provider_max_retries(Some(0));
    assert_eq!(settings.provider_retry_settings().max_retries, Some(0));
    settings.set_provider_timeout_ms(None);
    assert_eq!(read(&dirs.global())["retry"], json!({"provider": {"maxRetries": 0}}));
    settings.set_provider_max_retries(None);
    assert!(read(&dirs.global()).get("retry").is_none());
    assert_eq!(
        settings.provider_retry_settings(),
        ProviderRetryConfig { max_retry_delay_ms: 60_000, ..Default::default() }
    );
    settings.reload();
    assert_eq!(settings.provider_retry_settings().timeout_ms, None);
}

#[test]
fn disabled_providers_setter_writes_global_scope_only() {
    let dirs = Dirs::new();
    write(&dirs.project(), json!({"disabledProviders": ["project-only"]}));
    let settings = dirs.manager(true);
    assert!(settings.disabled_providers().is_empty());
    settings.set_disabled_providers(vec!["openai".into(), "groq".into()]);
    assert_eq!(read(&dirs.global())["disabledProviders"], json!(["openai", "groq"]));
    assert_eq!(read(&dirs.project())["disabledProviders"], json!(["project-only"]));
    assert_eq!(dirs.manager(true).disabled_providers(), vec!["openai", "groq"]);
    assert_eq!(super::disabled_providers(&dirs.agent), vec!["openai", "groq"]);
    settings.set_disabled_providers(Vec::new());
    assert!(read(&dirs.global()).get("disabledProviders").is_none());
    settings.reload();
    assert!(settings.disabled_providers().is_empty());
}
