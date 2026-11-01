//! Getters that return a stored object itself, as upstream `getEnabledModels` does: a change made
//! through it writes back to the layer the object lives in, until a setter or load replaces it.

use serde_json::{Map, Value, json};

use super::deferred::{layer, retype, sdk, slot};
use super::js::{defined, is_object_like, js_string, normalize, object};
use super::manager::{SettingsManager, State};
use super::storage::SettingsScope;

/// The merged path a getter returns as is, if it does.
pub(super) fn live_path(name: &str, args: &[Value]) -> Option<Vec<String>> {
    let path: &[&str] = match name {
        "getExtensionChangelogVersion" => {
            return Some(vec!["extensionChangelogVersions".into(), js_string(args.first())]);
        }
        "getModelThinkingLevel" => {
            let key = format!("{}/{}", js_string(args.first()), js_string(args.get(1)));
            return Some(vec!["modelThinkingLevels".into(), key]);
        }
        "getLastChangelogVersion" => &["lastChangelogVersion"],
        "getDefaultProvider" => &["defaultProvider"],
        "getDefaultModel" => &["defaultModel"],
        "getSteeringMode" => &["steeringMode"],
        "getFollowUpMode" => &["followUpMode"],
        "getDefaultThinkingLevel" => &["defaultThinkingLevel"],
        "getTransport" => &["transport"],
        "getCompactionEnabled" => &["compaction", "enabled"],
        "getBranchSummarySkipPrompt" => &["branchSummary", "skipPrompt"],
        "getRetryEnabled" => &["retry", "enabled"],
        "getHideThinkingBlock" => &["hideThinkingBlock"],
        "getShowCacheMissNotices" => &["showCacheMissNotices"],
        "getShellCommandPrefix" => &["shellCommandPrefix"],
        "getCollapseChangelog" => &["collapseChangelog"],
        "getEnableInstallTelemetry" => &["enableInstallTelemetry"],
        "getEnableAnalytics" => &["enableAnalytics"],
        "getTrackingId" => &["trackingId"],
        "getEnableSkillCommands" => &["enableSkillCommands"],
        "getThinkingBudgets" => &["thinkingBudgets"],
        "getShowImages" => &["terminal", "showImages"],
        "getClearOnShrink" => &["terminal", "clearOnShrink"],
        "getShowTerminalProgress" => &["terminal", "showTerminalProgress"],
        "getFullscreenCopyOnSelect" => &["fullscreenCopyOnSelect"],
        "getImageAutoResize" => &["images", "autoResize"],
        "getBlockImages" => &["images", "blockImages"],
        "getEnabledModels" => &["enabledModels"],
        "getDoubleEscapeAction" => &["doubleEscapeAction"],
        "getShowHardwareCursor" => &["showHardwareCursor"],
        "getEditorPaddingX" => &["editorPaddingX"],
        "getAutocompleteMaxVisible" => &["autocompleteMaxVisible"],
        "getCodeBlockIndent" => &["markdown", "codeBlockIndent"],
        _ => return None,
    };
    Some(path.iter().map(|part| part.to_string()).collect())
}

/// Where the merged object at `path` lives: a layer, or (`None`) only in the merged settings.
/// It folds the layers as `deepMergeObjects` does: two objects merge into a new one.
fn origin(state: &State, path: &[String]) -> Option<SettingsScope> {
    let sdk = state.sdk.as_ref()?;
    let layers = [(Some(SettingsScope::Global), &state.global), (Some(SettingsScope::Project), &state.project)];
    let all = layers.into_iter().chain(sdk.overrides.iter().map(|map| (None, map)));
    let first = path.first()?;
    let mut contributors: Vec<(Option<SettingsScope>, &Value)> =
        all.filter_map(|(scope, map)| defined(map.get(first)).map(|value| (scope, value))).collect();
    for depth in 0..path.len() {
        let mut parts: Vec<(Option<SettingsScope>, &Value)> = Vec::new();
        for (scope, value) in contributors {
            let into_last = parts.last().is_some_and(|(_, last)| object(Some(last)).is_some());
            if !(into_last && object(Some(value)).is_some()) {
                parts.clear();
            }
            parts.push((scope, value));
        }
        match parts.as_slice() {
            [] => return None,
            [(scope, _)] => return *scope,
            _ => {}
        }
        let next = path.get(depth + 1)?;
        contributors = parts
            .iter()
            .filter_map(|(scope, value)| defined(object(Some(value))?.get(next)).map(|child| (*scope, child)))
            .collect();
    }
    None
}

fn key(path: &[String]) -> String {
    path.join("\u{0}")
}

/// What must stay the same for an alias to still reach its object.
fn mark(state: &State, scope: Option<SettingsScope>, path: &[String]) -> Value {
    let Some(sdk) = state.sdk.as_ref() else { return Value::Null };
    let Some(scope) = scope else { return json!([sdk.merged_epoch]) };
    let index = slot(scope);
    let generation = |depth: usize| sdk.generations.get(&(index, key(&path[..depth]))).copied().unwrap_or(0);
    let marks: Vec<u64> = std::iter::once(sdk.epoch[index]).chain((1..=path.len()).map(generation)).collect();
    json!(marks)
}

/// The alias token of `path`, when the merged value there is an object.
pub(super) fn token(state: &State, path: &[String]) -> Option<Value> {
    let merged = &state.sdk.as_ref()?.merged;
    let (first, rest) = path.split_first()?;
    let value = rest.iter().try_fold(merged.get(first)?, |value, part| object(Some(value))?.get(part));
    if !is_object_like(value) {
        return None;
    }
    let scope = origin(state, path);
    Some(json!({"origin": scope.map(SettingsScope::as_str), "path": path, "mark": mark(state, scope, path)}))
}

/// A setter or load replaced the object at `path` of `scope`; aliases to it stop writing back.
pub(super) fn replaced(state: &mut State, scope: SettingsScope, path: &[&str]) {
    let path: Vec<String> = path.iter().map(|part| part.to_string()).collect();
    *sdk(state).generations.entry((slot(scope), key(&path))).or_default() += 1;
}

fn set_path(map: &mut Map<String, Value>, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else { return };
    let mut current = map;
    for part in parents {
        match current.get_mut(part) {
            Some(Value::Object(next)) => current = next,
            _ => return,
        }
    }
    current.insert(last.clone(), value);
}

impl SettingsManager {
    /// A change made through an alias: the new value of its object, written where it lives
    /// without marking the field or saving, as an in-place change upstream is.
    pub(crate) fn sdk_alias(&self, token: &Value, value: Value) {
        let mut state = self.lock();
        let parts = token["path"].as_array().into_iter().flatten();
        let path: Vec<String> = parts.filter_map(Value::as_str).map(String::from).collect();
        let scope = match token["origin"].as_str() {
            Some("global") => Some(SettingsScope::Global),
            Some("project") => Some(SettingsScope::Project),
            _ => None,
        };
        if path.is_empty() || mark(&state, scope, &path) != token["mark"] {
            return;
        }
        let value = normalize(value);
        if let Some(scope) = scope {
            set_path(layer(&mut state, scope), &path, value.clone());
        }
        if origin(&state, &path) == scope {
            set_path(&mut sdk(&mut state).merged, &path, value);
        }
        retype(&mut state);
    }
}
