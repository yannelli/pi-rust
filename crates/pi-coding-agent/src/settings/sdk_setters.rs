//! The upstream `SettingsManager` setters of an SDK manager: each changes the raw layer, marks the
//! field (and nested key) as modified, and saves, as `settings-manager.ts` does.

use serde_json::{Value, json};

use super::accessors_ui::uuid_v4;
use super::deferred::{UNTRUSTED, apply_overrides, mark, save, sdk};
use super::js::{as_number, js_string, nullish, number, object, read, spread, to_number, truthy, undefined};
use super::manager::State;
use super::sdk_alias::replaced;
use super::sdk_reads::array_copy;
use super::storage::SettingsScope;

/// `setX(value)`: `globalSettings[field] = value`.
const FIELDS: &[(&str, &str)] = &[
    ("setLastChangelogVersion", "lastChangelogVersion"),
    ("setDefaultProvider", "defaultProvider"),
    ("setDefaultModel", "defaultModel"),
    ("setSteeringMode", "steeringMode"),
    ("setFollowUpMode", "followUpMode"),
    ("setTheme", "theme"),
    ("setDefaultThinkingLevel", "defaultThinkingLevel"),
    ("setTransport", "transport"),
    ("setSessionShareDestination", "sessionShareDestination"),
    ("setCacheWarmingMode", "cacheWarming"),
    ("setHideThinkingBlock", "hideThinkingBlock"),
    ("setShowCacheMissNotices", "showCacheMissNotices"),
    ("setShellPath", "shellPath"),
    ("setQuietStartup", "quietStartup"),
    ("setDefaultProjectTrust", "defaultProjectTrust"),
    ("setShellCommandPrefix", "shellCommandPrefix"),
    ("setCollapseChangelog", "collapseChangelog"),
    ("setEnableInstallTelemetry", "enableInstallTelemetry"),
    ("setPackages", "packages"),
    ("setExtensionPaths", "extensions"),
    ("setSkillPaths", "skills"),
    ("setPromptTemplatePaths", "prompts"),
    ("setThemePaths", "themes"),
    ("setEnableSkillCommands", "enableSkillCommands"),
    ("setTuiMode", "tuiMode"),
    ("setFullscreenExitOutput", "fullscreenExitOutput"),
    ("setFullscreenScrollbar", "fullscreenScrollbar"),
    ("setFullscreenCopyOnSelect", "fullscreenCopyOnSelect"),
    ("setEnabledModels", "enabledModels"),
    ("setDoubleEscapeAction", "doubleEscapeAction"),
    ("setTreeFilterMode", "treeFilterMode"),
    ("setShowHardwareCursor", "showHardwareCursor"),
    ("setOutputPad", "outputPad"),
];

/// `setX(value)`: `globalSettings[field] ||= {}`, then `globalSettings[field][key] = value`.
const NESTED: &[(&str, &str, &str)] = &[
    ("setCompactionEnabled", "compaction", "enabled"),
    ("setRetryEnabled", "retry", "enabled"),
    ("setShowImages", "terminal", "showImages"),
    ("setClearOnShrink", "terminal", "clearOnShrink"),
    ("setShowTerminalProgress", "terminal", "showTerminalProgress"),
    ("setImageAutoResize", "images", "autoResize"),
    ("setBlockImages", "images", "blockImages"),
];

/// `setProjectX(value)`: upstream `updateProjectSettings(field, ...)`.
const PROJECT: &[(&str, &str)] = &[
    ("setProjectPackages", "packages"),
    ("setProjectExtensionPaths", "extensions"),
    ("setProjectSkillPaths", "skills"),
    ("setProjectPromptTemplatePaths", "prompts"),
    ("setProjectThemePaths", "themes"),
];

fn set(state: &mut State, field: &str, value: Value) {
    state.global.insert(field.to_string(), value);
    replaced(state, SettingsScope::Global, &[field]);
    mark(state, SettingsScope::Global, field, None);
}

/// `globalSettings[field][key] = value`, creating the object when the field is falsy (or, with
/// `nullish_only`, when it is `null` or `undefined`, as `??=` does).
fn set_nested(state: &mut State, field: &str, key: &str, value: Value, nullish_only: bool) {
    let current = state.global.get(field);
    let missing = if nullish_only { nullish(current) } else { !truthy(current) };
    if missing || object(current).is_none() {
        state.global.insert(field.to_string(), json!({}));
        replaced(state, SettingsScope::Global, &[field]);
    }
    if let Some(Value::Object(map)) = state.global.get_mut(field) {
        map.insert(key.to_string(), value);
    }
    replaced(state, SettingsScope::Global, &[field, key]);
    mark(state, SettingsScope::Global, field, Some(key));
}

/// `Math.max(low, Math.min(high, Math.floor(value)))`, `NaN` included.
fn clamp(value: Option<&Value>, low: f64, high: f64) -> Value {
    let floor = to_number(value).floor();
    number(if floor.is_nan() { floor } else { floor.clamp(low, high) })
}

fn generated(state: &mut State, field: &str) {
    if !truthy(state.global.get(field)) {
        set(state, field, Value::from(uuid_v4()));
    }
}

/// A setter (or another member that writes) by name; `None` when `name` is not one.
pub(super) fn setter(state: &mut State, name: &str, args: &[Value]) -> Result<Option<Value>, String> {
    let arg = |index: usize| args.get(index);
    let value = |index: usize| read(arg(index));
    if let Some((_, field)) = FIELDS.iter().find(|(setter, _)| *setter == name) {
        set(state, field, value(0));
    } else if let Some((_, field, key)) = NESTED.iter().find(|(setter, _, _)| *setter == name) {
        set_nested(state, field, key, value(0), false);
    } else if let Some((_, field)) = PROJECT.iter().find(|(setter, _)| *setter == name) {
        if !state.project_trusted {
            return Err(UNTRUSTED.into());
        }
        state.project.insert(field.to_string(), value(0));
        sdk(state).epoch[1] += 1;
        mark(state, SettingsScope::Project, field, None);
        save(state, SettingsScope::Project, false);
        return Ok(Some(undefined()));
    } else {
        match name {
            "applyOverrides" => {
                apply_overrides(state, object(arg(0)).cloned().unwrap_or_default());
                return Ok(Some(undefined()));
            }
            "getOrCreateDeviceId" => {
                if !truthy(state.global.get("deviceId")) {
                    generated(state, "deviceId");
                    save(state, SettingsScope::Global, false);
                }
                return Ok(Some(read(state.global.get("deviceId"))));
            }
            "setDefaultModelAndProvider" => {
                set(state, "defaultProvider", value(0));
                set(state, "defaultModel", value(1));
            }
            "setExtensionChangelogVersion" => {
                let key = js_string(arg(0));
                let mut versions = spread(state.global.get("extensionChangelogVersions"));
                versions.insert(key.clone(), value(1));
                state.global.insert("extensionChangelogVersions".into(), Value::Object(versions));
                replaced(state, SettingsScope::Global, &["extensionChangelogVersions"]);
                mark(state, SettingsScope::Global, "extensionChangelogVersions", Some(&key));
            }
            "setModelThinkingLevel" => {
                let key = format!("{}/{}", js_string(arg(0)), js_string(arg(1)));
                if !truthy(state.global.get("modelThinkingLevels")) {
                    state.global.insert("modelThinkingLevels".into(), json!({}));
                    replaced(state, SettingsScope::Global, &["modelThinkingLevels"]);
                }
                replaced(state, SettingsScope::Global, &["modelThinkingLevels", key.as_str()]);
                if let Some(Value::Object(levels)) = state.global.get_mut("modelThinkingLevels") {
                    levels.insert(key, value(2));
                }
                mark(state, SettingsScope::Global, "modelThinkingLevels", None);
            }
            "removeModelThinkingLevel" => {
                if !truthy(state.global.get("modelThinkingLevels")) {
                    return Ok(Some(undefined()));
                }
                let key = format!("{}/{}", js_string(arg(0)), js_string(arg(1)));
                let empty = match state.global.get_mut("modelThinkingLevels") {
                    Some(Value::Object(levels)) => {
                        levels.shift_remove(&key);
                        levels.is_empty()
                    }
                    _ => false,
                };
                replaced(state, SettingsScope::Global, &["modelThinkingLevels", key.as_str()]);
                if empty {
                    state.global.shift_remove("modelThinkingLevels");
                    replaced(state, SettingsScope::Global, &["modelThinkingLevels"]);
                }
                mark(state, SettingsScope::Global, "modelThinkingLevels", None);
            }
            "setHttpIdleTimeoutMs" => {
                let timeout = arg(0).and_then(as_number).filter(|ms| ms.is_finite() && *ms >= 0.0);
                let Some(timeout) = timeout else {
                    return Err(format!("Invalid httpIdleTimeoutMs setting: {}", js_string(arg(0))));
                };
                set(state, "httpIdleTimeoutMs", number(timeout.floor()));
            }
            "setNpmCommand" if truthy(arg(0)) => set(state, "npmCommand", array_copy(arg(0))?),
            "setNpmCommand" => set(state, "npmCommand", undefined()),
            "setEnableAnalytics" => {
                set(state, "enableAnalytics", value(0));
                if truthy(arg(0)) {
                    generated(state, "trackingId");
                }
            }
            "setImageWidthCells" => {
                let width = to_number(arg(0)).floor();
                let width = number(if width.is_nan() { width } else { width.max(1.0) });
                set_nested(state, "terminal", "imageWidthCells", width, false);
            }
            "setFullscreenWheelScrollLines" => {
                let auto = arg(0).and_then(Value::as_str) == Some("auto");
                let lines = if auto { value(0) } else { clamp(arg(0), 1.0, 100.0) };
                set(state, "fullscreenWheelScrollLines", lines);
            }
            "setEditorPaddingX" => set(state, "editorPaddingX", clamp(arg(0), 0.0, 3.0)),
            "setAutocompleteMaxVisible" => set(state, "autocompleteMaxVisible", clamp(arg(0), 3.0, 20.0)),
            "setMermaidRenderingMode" => set_nested(state, "markdown", "mermaid", value(0), true),
            "setWarnings" => set(state, "warnings", Value::Object(spread(arg(0)))),
            _ => return Ok(None),
        }
    }
    save(state, SettingsScope::Global, false);
    Ok(Some(undefined()))
}
