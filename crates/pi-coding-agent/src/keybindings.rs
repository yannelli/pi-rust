//! App keybindings (upstream `core/keybindings.ts`): the `app.*` ids, platform defaults, the
//! legacy-name migration, and `keybindings.json` loading. Components read the process-wide
//! manager through [`pi_tui::get_keybindings`], so call [`KeybindingsManager::install`] at startup.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_tui::keybindings::{
    KeybindingDefinition, KeybindingDefinitions, KeybindingValue, KeybindingsConfig, tui_keybindings,
};
use serde_json::{Map, Value};

/// Whether the Windows defaults apply: native Windows, or Linux under WSL.
pub fn use_windows_keybindings_for(platform: &str, env: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    platform == "win32" || (platform == "linux" && (set("WSL_DISTRO_NAME") || set("WSL_INTEROP")))
}

/// The Node.js `process.platform` name of this build target.
pub fn current_platform() -> &'static str {
    if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    }
}

/// [`use_windows_keybindings_for`] with this process's platform and environment.
pub fn use_windows_keybindings() -> bool {
    use_windows_keybindings_for(current_platform(), |name| std::env::var(name).ok())
}

fn def(keys: &[&str], description: &str) -> KeybindingDefinition {
    KeybindingDefinition::new(keys, description)
}

/// The `app.*` keybindings (upstream `APP_KEYBINDINGS`), in upstream order.
pub fn app_keybindings_for(platform: &str, windows: bool) -> KeybindingDefinitions {
    let pick = |on_windows: &'static str, otherwise: &'static str| if windows { on_windows } else { otherwise };
    let mac = platform == "darwin";
    let fold = if mac { ["alt+left", "ctrl+left"] } else { ["ctrl+left", "alt+left"] };
    let unfold = if mac { ["alt+right", "ctrl+right"] } else { ["ctrl+right", "alt+right"] };
    let suspend: &[&str] = if platform == "win32" { &[] } else { &["ctrl+z"] };
    [
        ("app.interrupt", def(&["escape"], "Cancel or abort")),
        ("app.clear", def(&["ctrl+c"], "Clear editor")),
        ("app.exit", def(&["ctrl+d"], "Exit when editor is empty")),
        ("app.suspend", def(suspend, "Suspend to background")),
        ("app.thinking.cycle", def(&["shift+tab"], "Cycle thinking level")),
        ("app.thinking.save", def(&["ctrl+s"], "Save thinking level")),
        ("app.model.cycleForward", def(&["ctrl+p"], "Cycle to next model")),
        ("app.model.cycleBackward", def(&[pick("alt+p", "shift+ctrl+p")], "Cycle to previous model")),
        ("app.model.select", def(&["ctrl+l"], "Open model selector")),
        ("app.tools.expand", def(&["ctrl+o"], "Toggle tool output")),
        ("app.thinking.toggle", def(&["ctrl+t"], "Toggle thinking blocks")),
        ("app.session.toggleNamedFilter", def(&["ctrl+n"], "Toggle named session filter")),
        ("app.editor.external", def(&["ctrl+g"], "Open external editor")),
        ("app.message.copy", def(&["ctrl+x"], "Copy selection or last assistant message")),
        ("app.message.followUp", def(&[pick("ctrl+q", "alt+enter")], "Queue follow-up message")),
        ("app.message.dequeue", def(&[pick("alt+q", "alt+up")], "Restore queued messages")),
        (
            "app.clipboard.pasteImage",
            def(&[pick("alt+v", "ctrl+v")], "Paste files on macOS, images, or text from clipboard"),
        ),
        ("app.session.new", def(&[], "Start a new session")),
        ("app.session.tree", def(&[], "Open session tree")),
        ("app.session.fork", def(&[], "Fork current session")),
        ("app.session.resume", def(&[], "Resume a session")),
        ("app.tree.foldOrUp", def(&fold, "Fold tree branch or move up")),
        ("app.tree.unfoldOrDown", def(&unfold, "Unfold tree branch or move down")),
        ("app.tree.editLabel", def(&["shift+l"], "Edit tree label")),
        ("app.tree.toggleLabelTimestamp", def(&["shift+t"], "Toggle tree label timestamps")),
        ("app.session.togglePath", def(&["ctrl+p"], "Toggle session path display")),
        ("app.session.toggleSort", def(&["ctrl+s"], "Toggle session sort mode")),
        ("app.session.rename", def(&["ctrl+r"], "Rename session")),
        ("app.session.delete", def(&["ctrl+d"], "Delete session")),
        ("app.session.deleteNoninvasive", def(&["ctrl+backspace"], "Delete session when query is empty")),
        ("app.models.save", def(&["ctrl+s"], "Save model selection")),
        ("app.models.enableAll", def(&["ctrl+a"], "Enable all models")),
        ("app.models.clearAll", def(&["ctrl+x"], "Clear all models")),
        ("app.models.toggleProvider", def(&["ctrl+p"], "Toggle all models for provider")),
        ("app.models.reorderUp", def(&["alt+up"], "Move model up in order")),
        ("app.models.reorderDown", def(&["alt+down"], "Move model down in order")),
        ("app.tree.filter.default", def(&["ctrl+d"], "Tree filter: default view")),
        ("app.tree.filter.noTools", def(&["ctrl+t"], "Tree filter: hide tool results")),
        ("app.tree.filter.userOnly", def(&["ctrl+u"], "Tree filter: user messages only")),
        ("app.tree.filter.labeledOnly", def(&["ctrl+l"], "Tree filter: labeled entries only")),
        ("app.tree.filter.all", def(&["ctrl+a"], "Tree filter: show all entries")),
        ("app.tree.filter.cycleForward", def(&["ctrl+o"], "Tree filter: cycle forward")),
        ("app.tree.filter.cycleBackward", def(&["shift+ctrl+o"], "Tree filter: cycle backward")),
    ]
    .into_iter()
    .collect()
}

/// Every keybinding of the coding agent (upstream `KEYBINDINGS`): the TUI set with the platform
/// overrides, followed by the app set.
pub fn keybindings_for(platform: &str, windows: bool) -> KeybindingDefinitions {
    let mut definitions = tui_keybindings();
    let mut set_keys = |id: &str, keys: &[&str]| {
        if let Some(definition) = definitions.get_mut(id) {
            definition.default_keys = keys.iter().map(|key| (*key).to_string()).collect();
        }
    };
    let undo = if platform == "win32" {
        "ctrl+z"
    } else if windows {
        "alt+z"
    } else {
        "ctrl+-"
    };
    set_keys("tui.editor.undo", &[undo]);
    let (previous, next, search): (&[&str], &[&str], &[&str]) = if windows {
        (&["ctrl+up"], &["ctrl+down"], &["ctrl+f"])
    } else {
        (&["ctrl+shift+up", "ctrl+up"], &["ctrl+shift+down", "ctrl+down"], &["ctrl+shift+f"])
    };
    set_keys("tui.altScreen.previousPrompt", previous);
    set_keys("tui.altScreen.nextPrompt", next);
    set_keys("tui.altScreen.search", search);
    definitions.extend(app_keybindings_for(platform, windows));
    definitions
}

/// [`keybindings_for`] with this process's platform.
pub fn keybindings() -> KeybindingDefinitions {
    keybindings_for(current_platform(), use_windows_keybindings())
}

/// Legacy `keybindings.json` names and their namespaced ids (upstream `KEYBINDING_NAME_MIGRATIONS`).
pub const KEYBINDING_NAME_MIGRATIONS: &[(&str, &str)] = &[
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

fn migrated_name(key: &str) -> Option<&'static str> {
    KEYBINDING_NAME_MIGRATIONS.iter().find(|(old, _)| *old == key).map(|(_, new)| *new)
}

/// Renames legacy keys to namespaced ids and orders the result like [`keybindings`], with
/// unknown keys sorted at the end. Returns the config and whether anything changed.
pub fn migrate_keybindings_config(raw: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut config = Map::new();
    let mut migrated = false;
    for (key, value) in raw {
        let next = migrated_name(key).unwrap_or(key);
        if next != key {
            migrated = true;
            if raw.contains_key(next) {
                continue;
            }
        }
        config.insert(next.to_string(), value.clone());
    }
    (order_keybindings_config(config), migrated)
}

fn order_keybindings_config(mut config: Map<String, Value>) -> Map<String, Value> {
    let mut ordered = Map::new();
    for id in keybindings().ids() {
        if let Some(value) = config.remove(id) {
            ordered.insert(id.to_string(), value);
        }
    }
    let mut extras: Vec<(String, Value)> = config.into_iter().collect();
    extras.sort_by(|a, b| a.0.cmp(&b.0));
    ordered.extend(extras);
    ordered
}

const KEY_MODIFIERS: [&str; 4] = ["ctrl", "shift", "alt", "super"];
const NAMED_KEYS: &[&str] = &[
    "escape",
    "esc",
    "enter",
    "return",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "clear",
    "home",
    "end",
    "pageUp",
    "pageDown",
    "up",
    "down",
    "left",
    "right",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "`",
    "-",
    "=",
    "[",
    "]",
    "\\",
    ";",
    "'",
    ",",
    ".",
    "/",
    "!",
    "@",
    "#",
    "$",
    "%",
    "^",
    "&",
    "*",
    "(",
    ")",
    "_",
    "+",
    "|",
    "~",
    "{",
    "}",
    ":",
    "<",
    ">",
    "?",
];

/// Whether `key` is a key id with up to four distinct modifiers (upstream `KeyIdSchema`).
pub fn is_valid_key_id(key: &str) -> bool {
    let repeats = KEY_MODIFIERS.iter().any(|modifier| {
        let prefix = format!("{modifier}+");
        key.find(&prefix).is_some_and(|at| key[at + prefix.len()..].contains(&prefix))
    });
    if repeats {
        return false;
    }
    let mut rest = key;
    for _ in 0..4 {
        match KEY_MODIFIERS.iter().find_map(|modifier| rest.strip_prefix(modifier)?.strip_prefix('+')) {
            Some(stripped) if !stripped.is_empty() => rest = stripped,
            _ => break,
        }
    }
    let single = rest.len() == 1 && rest.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    single || NAMED_KEYS.contains(&rest)
}

fn keybinding_value(value: &Value) -> Option<KeybindingValue> {
    match value {
        Value::String(key) if is_valid_key_id(key) => Some(KeybindingValue::One(key.clone())),
        Value::Array(keys) => keys
            .iter()
            .map(|key| key.as_str().filter(|key| is_valid_key_id(key)).map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(KeybindingValue::Many),
        _ => None,
    }
}

fn to_keybindings_config(raw: &Map<String, Value>) -> KeybindingsConfig {
    raw.iter()
        .filter(|(key, _)| key.as_str() != "$schema")
        .filter_map(|(key, value)| Some((key.clone(), keybinding_value(value)?)))
        .collect()
}

fn load_raw_config(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// `keybindings.json` in the agent directory.
pub fn keybindings_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("keybindings.json")
}

/// Reads user overrides from a `keybindings.json`, migrating legacy names in memory and dropping
/// invalid values. A missing or malformed file yields no overrides.
pub fn load_keybindings_file(path: &Path) -> KeybindingsConfig {
    load_raw_config(path).map(|raw| to_keybindings_config(&migrate_keybindings_config(&raw).0)).unwrap_or_default()
}

/// Rewrites legacy names in `<agent dir>/keybindings.json` (upstream `migrateKeybindingsConfigFile`).
pub fn migrate_keybindings_config_file(agent_dir: &Path) {
    let path = keybindings_path(agent_dir);
    let Some(raw) = load_raw_config(&path) else { return };
    let (config, migrated) = migrate_keybindings_config(&raw);
    if migrated && let Ok(text) = serde_json::to_string_pretty(&Value::Object(config)) {
        let _ = std::fs::write(&path, format!("{text}\n"));
    }
}

/// The coding agent's keybindings: [`keybindings`] plus the user's `keybindings.json`.
#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    inner: pi_tui::KeybindingsManager,
    config_path: Option<PathBuf>,
}

impl KeybindingsManager {
    pub fn new(user_bindings: KeybindingsConfig, config_path: Option<PathBuf>) -> Self {
        Self { inner: pi_tui::KeybindingsManager::new(keybindings(), user_bindings), config_path }
    }

    /// Loads `<agent dir>/keybindings.json`.
    pub fn create(agent_dir: &Path) -> Self {
        let config_path = keybindings_path(agent_dir);
        let user_bindings = load_keybindings_file(&config_path);
        Self::new(user_bindings, Some(config_path))
    }

    /// Re-reads the config file (for `/reload`). Call [`Self::install`] afterwards.
    pub fn reload(&mut self) {
        if let Some(path) = &self.config_path {
            self.inner.set_user_bindings(load_keybindings_file(path));
        }
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// The effective keys of every id.
    pub fn get_effective_config(&self) -> KeybindingsConfig {
        self.inner.get_resolved_bindings()
    }

    /// The TUI manager holding the definitions and overrides.
    pub fn manager(&self) -> &pi_tui::KeybindingsManager {
        &self.inner
    }

    /// Makes these bindings the process-wide ones that components read.
    pub fn install(&self) -> Arc<pi_tui::KeybindingsManager> {
        pi_tui::set_keybindings(self.inner.clone());
        pi_tui::get_keybindings()
    }
}

impl Default for KeybindingsManager {
    fn default() -> Self {
        Self::new(KeybindingsConfig::default(), None)
    }
}

impl Deref for KeybindingsManager {
    type Target = pi_tui::KeybindingsManager;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Re-reads `<agent dir>/keybindings.json` into the process-wide bindings (for `/reload`).
pub fn reload_installed_keybindings(agent_dir: &Path) -> Arc<pi_tui::KeybindingsManager> {
    KeybindingsManager::create(agent_dir).install()
}

#[cfg(test)]
mod tests;
