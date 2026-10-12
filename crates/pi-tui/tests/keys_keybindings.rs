//! Port of upstream keybindings.test.ts, plus checks for the JSON config shape, extension by
//! callers, resolved bindings, and the process-wide manager.

use std::sync::MutexGuard;

use pi_tui::env::global_state_lock;
use pi_tui::keybindings::{
    KeybindingConflict, KeybindingDefinition, KeybindingValue, KeybindingsConfig, KeybindingsManager, get_keybindings,
    ids, set_keybindings, tui_keybindings, update_keybindings,
};
use pi_tui::keys::set_kitty_protocol_active;

fn locked() -> MutexGuard<'static, ()> {
    let guard = global_state_lock();
    set_kitty_protocol_active(false);
    guard
}

fn manager(user: KeybindingsConfig) -> KeybindingsManager {
    KeybindingsManager::new(tui_keybindings(), user)
}

fn keys(manager: &KeybindingsManager, id: &str) -> Vec<String> {
    manager.get_keys(id)
}

#[test]
fn binds_ctrl_j_as_default_newline_alias() {
    let _lock = locked();
    let kb = manager(KeybindingsConfig::new());
    assert_eq!(keys(&kb, "tui.input.newLine"), ["shift+enter", "ctrl+j"]);
    assert!(kb.matches("\n", "tui.input.newLine"));
    assert!(kb.matches("\x1b[106;5u", "tui.input.newLine"));
}

#[test]
fn binds_modified_and_unmodified_editor_viewport_navigation() {
    let kb = manager(KeybindingsConfig::new());
    assert_eq!(keys(&kb, "tui.editor.cursorLineStart"), ["home", "ctrl+a"]);
    assert_eq!(keys(&kb, "tui.editor.cursorLineEnd"), ["end", "ctrl+e"]);
    assert_eq!(keys(&kb, "tui.editor.pageUp"), ["pageUp", "ctrl+pageUp"]);
    assert_eq!(keys(&kb, "tui.editor.pageDown"), ["pageDown", "ctrl+pageDown"]);
}

#[test]
fn leaves_dedicated_prompt_history_navigation_unbound_by_default() {
    let kb = manager(KeybindingsConfig::new());
    assert!(keys(&kb, "tui.editor.historyPrevious").is_empty());
    assert!(keys(&kb, "tui.editor.historyNext").is_empty());
}

#[test]
fn binds_unmodified_terminal_viewport_shortcuts_to_alternate_screen_navigation() {
    let kb = manager(KeybindingsConfig::new());
    let expected: [(&str, &[&str]); 14] = [
        ("tui.altScreen.pageUp", &["pageUp"]),
        ("tui.altScreen.pageDown", &["pageDown"]),
        ("tui.altScreen.halfPageUp", &[]),
        ("tui.altScreen.halfPageDown", &[]),
        ("tui.altScreen.lineUp", &[]),
        ("tui.altScreen.lineDown", &[]),
        ("tui.altScreen.previousPrompt", &["ctrl+shift+up", "ctrl+up"]),
        ("tui.altScreen.nextPrompt", &["ctrl+shift+down", "ctrl+down"]),
        ("tui.altScreen.search", &["ctrl+shift+f"]),
        ("tui.altScreen.searchNext", &["enter", "ctrl+g"]),
        ("tui.altScreen.searchPrevious", &["shift+enter", "ctrl+shift+g"]),
        ("tui.altScreen.searchClose", &["escape"]),
        ("tui.altScreen.top", &["ctrl+home"]),
        ("tui.altScreen.bottom", &["ctrl+end"]),
    ];
    for (id, expected_keys) in expected {
        assert_eq!(keys(&kb, id), expected_keys, "{id}");
    }
}

#[test]
fn does_not_evict_selector_confirm_when_input_submit_is_rebound() {
    let kb = manager([("tui.input.submit", ["enter", "ctrl+enter"])].into_iter().collect());
    assert_eq!(keys(&kb, "tui.input.submit"), ["enter", "ctrl+enter"]);
    assert_eq!(keys(&kb, "tui.select.confirm"), ["enter"]);
}

#[test]
fn does_not_evict_cursor_bindings_when_another_action_reuses_the_same_key() {
    let kb = manager([("tui.select.up", ["up", "ctrl+p"])].into_iter().collect());
    assert_eq!(keys(&kb, "tui.select.up"), ["up", "ctrl+p"]);
    assert_eq!(keys(&kb, "tui.editor.cursorUp"), ["up"]);
}

#[test]
fn still_reports_direct_user_binding_conflicts_without_evicting_defaults() {
    let kb = manager([("tui.input.submit", "ctrl+x"), ("tui.select.confirm", "ctrl+x")].into_iter().collect());
    assert_eq!(
        kb.get_conflicts(),
        [KeybindingConflict {
            key: "ctrl+x".to_string(),
            keybindings: vec!["tui.input.submit".to_string(), "tui.select.confirm".to_string()],
        }]
    );
    assert_eq!(keys(&kb, "tui.editor.cursorLeft"), ["left", "ctrl+b"]);
}

#[test]
fn config_round_trips_the_keybindings_json_shape_in_order() {
    let json = r#"{"tui.select.up":["up","ctrl+p"],"app.session.new":"ctrl+shift+n","tui.altScreen.pageUp":[]}"#;
    let config: KeybindingsConfig = serde_json::from_str(json).expect("valid config");
    let ids: Vec<&str> = config.iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["tui.select.up", "app.session.new", "tui.altScreen.pageUp"]);
    assert_eq!(config.get("app.session.new"), Some(&KeybindingValue::One("ctrl+shift+n".to_string())));
    assert_eq!(serde_json::to_string(&config).expect("serializes"), json);

    let duplicate: KeybindingsConfig = serde_json::from_str(r#"{"a":"x","b":"y","a":"z"}"#).expect("valid config");
    let pairs: Vec<(&str, &[String])> = duplicate.iter().map(|(id, value)| (id, value.keys())).collect();
    assert_eq!(pairs, [("a", &["z".to_string()][..]), ("b", &["y".to_string()][..])]);
    assert!(serde_json::from_str::<KeybindingsConfig>(r#"{"a":1}"#).is_err());
}

#[test]
fn empty_list_disables_and_unknown_ids_are_ignored() {
    let _lock = locked();
    let user: KeybindingsConfig =
        [("tui.altScreen.pageUp", vec![]), ("unknown.id", vec!["ctrl+x"]), ("tui.input.copy", vec!["ctrl+x"])]
            .into_iter()
            .collect();
    let kb = manager(user);
    assert!(keys(&kb, "tui.altScreen.pageUp").is_empty());
    assert!(!kb.matches("\x1b[5~", "tui.altScreen.pageUp"));
    assert!(kb.get_conflicts().is_empty());
    assert!(keys(&kb, "unknown.id").is_empty());
    assert!(kb.get_definition("unknown.id").is_none());
    assert_eq!(kb.get_definition(ids::INPUT_COPY).and_then(|d| d.description.as_deref()), Some("Copy selection"));
}

#[test]
fn user_keys_are_deduplicated() {
    let kb = manager([("tui.select.up", ["up", "up", "ctrl+p"])].into_iter().collect());
    assert_eq!(keys(&kb, "tui.select.up"), ["up", "ctrl+p"]);
}

#[test]
fn resolved_bindings_use_a_string_for_one_key_and_a_list_otherwise() {
    let kb = manager([("tui.input.submit", vec!["ctrl+enter"])].into_iter().collect());
    let resolved = kb.get_resolved_bindings();
    assert_eq!(resolved.len(), tui_keybindings().len());
    assert_eq!(resolved.get(ids::INPUT_SUBMIT), Some(&KeybindingValue::One("ctrl+enter".to_string())));
    assert_eq!(resolved.get(ids::EDITOR_HISTORY_NEXT), Some(&KeybindingValue::Many(vec![])));
    let json = serde_json::to_value(&resolved).expect("serializes");
    assert_eq!(json["tui.editor.cursorLeft"], serde_json::json!(["left", "ctrl+b"]));
    assert_eq!(kb.get_user_bindings().len(), 1);
}

#[test]
fn tui_definitions_cover_every_id_in_order() {
    let definitions = tui_keybindings();
    assert_eq!(ids::ALL.len(), 47);
    assert_eq!(definitions.ids().collect::<Vec<_>>(), ids::ALL);
    assert_eq!(definitions.get(ids::EDITOR_UNDO).map(|d| d.default_keys.clone()), Some(vec!["ctrl+-".to_string()]));
}

#[test]
fn callers_extend_definitions_and_overrides_keep_position() {
    let mut definitions = tui_keybindings();
    let undo_position = definitions.ids().position(|id| id == ids::EDITOR_UNDO);
    definitions.insert(ids::EDITOR_UNDO, KeybindingDefinition::new(&["ctrl+-", "ctrl+z"], "Undo"));
    definitions.insert("app.session.new", KeybindingDefinition::new(&[], "Start a new session"));
    assert_eq!(definitions.ids().position(|id| id == ids::EDITOR_UNDO), undo_position);
    assert_eq!(definitions.ids().last(), Some("app.session.new"));

    let mut kb = KeybindingsManager::new(definitions, KeybindingsConfig::new());
    assert_eq!(keys(&kb, ids::EDITOR_UNDO), ["ctrl+-", "ctrl+z"]);
    kb.set_user_bindings([("app.session.new", "ctrl+shift+n")].into_iter().collect());
    assert_eq!(keys(&kb, "app.session.new"), ["ctrl+shift+n"]);
}

#[test]
fn global_manager_defaults_to_tui_bindings_and_can_be_replaced() {
    let _lock = locked();
    set_keybindings(KeybindingsManager::default());
    assert_eq!(get_keybindings().get_keys(ids::SELECT_CANCEL), ["escape", "ctrl+c"]);

    set_keybindings(manager([(ids::SELECT_CANCEL, "ctrl+q")].into_iter().collect()));
    assert_eq!(get_keybindings().get_keys(ids::SELECT_CANCEL), ["ctrl+q"]);
    assert!(get_keybindings().matches("\x11", ids::SELECT_CANCEL));

    let updated = update_keybindings(|kb| kb.set_user_bindings(KeybindingsConfig::new()));
    assert_eq!(updated.get_keys(ids::SELECT_CANCEL), ["escape", "ctrl+c"]);
    assert_eq!(get_keybindings().get_keys(ids::SELECT_CANCEL), ["escape", "ctrl+c"]);
}
