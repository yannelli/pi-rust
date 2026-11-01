//! An SDK manager shared with a child session: the caller's members and the native setters
//! `AgentSession` calls queue snapshots in one order, and storage sees nothing until the queue runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_ai::ThinkingLevel;
use serde_json::{Map, Value, json};

use super::deferred::UNTRUSTED;
use super::js::undefined;
use super::*;

struct Shared {
    _root: tempfile::TempDir,
    global: std::path::PathBuf,
    manager: Arc<SettingsManager>,
    wakes: Arc<AtomicUsize>,
}

fn shared(global: Value) -> Shared {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent) = (root.path().join("cwd"), root.path().join("agent"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("settings.json"), serde_json::to_string_pretty(&global).unwrap()).unwrap();
    let manager = Arc::new(SettingsManager::sdk(Arc::new(FileSettingsStorage::new(&cwd, &agent)), true, None));
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    manager.sdk_watch(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    Shared { global: agent.join("settings.json"), _root: root, manager, wakes }
}

fn call(manager: &SettingsManager, name: &str, args: &[Value]) -> Value {
    manager.sdk_call(name, args, &Map::new()).unwrap().expect("an upstream member").0
}

fn host(global: &str) -> SettingsManager {
    let reads = (Ok(Some(json!(global))), Some(Ok(None)));
    SettingsManager::sdk(Arc::new(HostStorage), true, Some(reads))
}

#[test]
fn child_writes_queue_behind_caller_writes_and_wake_the_queue() {
    let shared = shared(json!({"unrelated": 1}));
    let before = std::fs::read_to_string(&shared.global).unwrap();
    call(&shared.manager, "setTheme", &[json!("light")]);
    assert_eq!(shared.wakes.load(Ordering::SeqCst), 0, "a JavaScript call claims its own snapshot");
    assert_eq!(shared.manager.sdk_claim(), 1);

    let child = shared.manager.clone();
    std::thread::spawn(move || child.set_default_model_and_provider("openai", "gpt-child")).join().unwrap();
    let queued = shared.manager.sdk_claim();
    assert_eq!(queued, 1, "model and provider share one saved snapshot");
    assert_eq!(shared.wakes.load(Ordering::SeqCst), queued, "each child snapshot wakes the queue");
    assert_eq!(std::fs::read_to_string(&shared.global).unwrap(), before, "nothing is written before the queue runs");
    assert_eq!(shared.manager.theme().as_deref(), Some("light"), "the child reads the caller's change at once");
    assert_eq!(call(&shared.manager, "getDefaultModel", &[]), json!("gpt-child"));

    for _ in 0..1 + queued {
        shared.manager.sdk_persist();
    }
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&shared.global).unwrap()).unwrap();
    let expected = json!({"unrelated": 1, "theme": "light", "defaultProvider": "openai", "defaultModel": "gpt-child"});
    assert_eq!(serde_json::to_string(&file).unwrap(), serde_json::to_string(&expected).unwrap());
    assert!(shared.manager.drain_errors().is_empty());
}

#[test]
fn a_queued_child_write_keeps_an_external_edit() {
    let shared = shared(json!({"a": 1}));
    shared.manager.set_compaction_enabled(false);
    std::fs::write(&shared.global, r#"{"a": 2, "compaction": {"reserveTokens": 5}, "external": true}"#).unwrap();
    assert_eq!(shared.manager.sdk_claim(), 1);
    shared.manager.sdk_persist();
    let text = std::fs::read_to_string(&shared.global).unwrap();
    let expected = json!({"a": 2, "compaction": {"reserveTokens": 5, "enabled": false}, "external": true});
    assert_eq!(text, serde_json::to_string_pretty(&expected).unwrap());
    assert!(!shared.manager.compaction_enabled());
}

#[test]
fn a_host_storage_write_merges_into_the_text_javascript_read() {
    let manager = host(r#"{"a": 1}"#);
    manager.set_default_thinking_level(ThinkingLevel::High);
    manager.set_compaction_enabled(false);
    assert_eq!(manager.sdk_claim(), 2);
    for current in [r#"{"a": 2}"#, r#"{"a": 2, "defaultThinkingLevel": "high", "compaction": {"keep": 1}}"#] {
        let (scope, refused) = manager.sdk_take().unwrap();
        assert_eq!((scope, refused), (SettingsScope::Global, None));
        let text = manager.sdk_text(Some(json!(current))).unwrap();
        assert!(text.contains("\"defaultThinkingLevel\": \"high\""), "{text}");
        manager.sdk_done(scope);
    }
    assert!(manager.sdk_take().is_none());
}

#[test]
fn project_writes_check_trust_when_they_run() {
    let manager = host("{}");
    call(&manager, "setProjectSkillPaths", &[json!(["./s"])]);
    assert_eq!(manager.sdk_claim(), 1);
    manager.sdk_set_trusted(false, None);
    assert_eq!(manager.sdk_take(), Some((SettingsScope::Project, Some(UNTRUSTED))));
    assert_eq!(manager.sdk_call("setProjectSkillPaths", &[json!([])], &Map::new()), Err(UNTRUSTED.to_string()));
}

#[test]
fn a_returned_stored_object_writes_back_until_a_setter_replaces_it() {
    let manager = host(r#"{"enabledModels": ["a/*"], "thinkingBudgets": {"low": 1}}"#);
    let (models, alias) = manager.sdk_call("getEnabledModels", &[], &Map::new()).unwrap().unwrap();
    let alias = alias.expect("a stored array is returned as is");
    assert_eq!((models, alias["origin"].clone()), (json!(["a/*"]), json!("global")));
    manager.sdk_alias(&alias, json!(["a/*", "pushed"]));
    assert_eq!(call(&manager, "getGlobalSettings", &[])["enabledModels"], json!(["a/*", "pushed"]));
    assert_eq!(manager.enabled_models(), Some(vec!["a/*".to_string(), "pushed".to_string()]));
    assert_eq!(manager.sdk_claim(), 0, "an in-place change marks and saves nothing");
    call(&manager, "setEnabledModels", &[json!(["b/*"])]);
    manager.sdk_alias(&alias, json!(["stale"]));
    assert_eq!(call(&manager, "getEnabledModels", &[]), json!(["b/*"]), "the replaced object no longer writes back");
    let number = manager.sdk_call("getDefaultModel", &[], &Map::new()).unwrap().unwrap();
    assert_eq!(number.1, None, "values that are not objects have no alias");
}

#[test]
fn a_scope_that_failed_to_load_is_never_written() {
    let manager = host("{");
    let errors = manager.drain_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!((errors[0].scope, errors[0].path.as_ref()), (SettingsScope::Global, None));
    manager.set_theme("dark");
    assert_eq!(manager.sdk_claim(), 0, "upstream save skips a scope with a load error");
    assert_eq!(manager.theme().as_deref(), Some("dark"));
}

#[test]
fn undefined_values_and_overrides_follow_upstream() {
    let manager = host(r#"{"shellPath": "/bin/sh", "theme": "dark"}"#);
    call(&manager, "setShellPath", &[undefined()]);
    let global = call(&manager, "getGlobalSettings", &[]);
    assert_eq!(global["shellPath"], undefined(), "the key stays, holding undefined");
    assert_eq!(manager.shell_path(), None, "native readers see no value");
    manager.sdk_claim();
    manager.sdk_take();
    let current = json!(r#"{"shellPath": "/bin/sh", "theme": "dark"}"#);
    assert_eq!(manager.sdk_text(Some(current)).unwrap(), "{\n  \"theme\": \"dark\"\n}", "JSON.stringify drops it");

    call(&manager, "applyOverrides", &[json!({"theme": "override"})]);
    assert_eq!(manager.theme().as_deref(), Some("override"));
    manager.set_default_thinking_level(ThinkingLevel::Low);
    assert_eq!(manager.theme().as_deref(), Some("dark"), "the next save drops overrides, as upstream's does");
}

#[test]
fn startup_diagnostics_keep_schema_warnings_apart() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("settings.json"), r#"{"tuiMode": "sideways"}"#).unwrap();
    let settings = SettingsManager::create(root.path(), root.path(), SettingsOptions { project_trusted: false });
    assert!(settings.drain_errors().is_empty(), "schema warnings are not storage errors");
    let diagnostics = settings.drain_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("\"tuiMode\""), "{diagnostics:?}");
}
