//! The upstream `SettingsManager` getters of an SDK manager, read from the raw layers with the
//! JavaScript rules of `settings-manager.ts`: `??` and `||` defaults, clamps, copies and throws.

use serde_json::{Map, Value, json};

use super::js::{as_number, defined, js_string, nullish, number, object, or, read, spread, truthy, undefined};
use super::manager::{SettingsManager, State};
use super::sdk_alias::{live_path, token};
use super::sdk_reads::{
    array_copy, at, compaction_tokens, default_tools, editor, one_of, path_value, terminal_overrides, timeout,
};

fn compaction(merged: &Map<String, Value>, model: Option<&Value>) -> Result<Value, String> {
    Ok(json!({
        "enabled": or(at(merged, &["compaction", "enabled"]), json!(true)),
        "reserveTokens": compaction_tokens(merged, "reserveTokens", model)?,
        "keepRecentTokens": compaction_tokens(merged, "keepRecentTokens", model)?,
    }))
}

fn flag(env: &Map<String, Value>, key: &str) -> Value {
    Value::Bool(env.get(key).and_then(Value::as_str) == Some("1"))
}

/// A getter of upstream `SettingsManager` by name; `None` when `name` is not one.
pub(super) fn getter(
    state: &State,
    name: &str,
    args: &[Value],
    env: &Map<String, Value>,
) -> Result<Option<Value>, String> {
    let empty = Map::new();
    let merged = state.sdk.as_ref().map_or(&empty, |sdk| &sdk.merged);
    let v = |path: &[&str]| at(merged, path);
    let arg = |index: usize| args.get(index);
    let finite = |value: Option<&Value>| defined(value).and_then(as_number).filter(|n| n.is_finite());
    Ok(Some(match name {
        "getSettings" => Value::Object(merged.clone()),
        "getGlobalSettings" => Value::Object(state.global.clone()),
        "getProjectSettings" => Value::Object(state.project.clone()),
        "isProjectTrusted" => Value::Bool(state.project_trusted),
        "getLastChangelogVersion" => read(v(&["lastChangelogVersion"])),
        "getExtensionChangelogVersion" => {
            read(object(v(&["extensionChangelogVersions"])).and_then(|map| map.get(&js_string(arg(0)))))
        }
        "getSessionDir" => path_value(v(&["sessionDir"]), env),
        "getDefaultProvider" => read(v(&["defaultProvider"])),
        "getDefaultModel" => read(v(&["defaultModel"])),
        "getSteeringMode" | "getFollowUpMode" => {
            let key = if name == "getSteeringMode" { "steeringMode" } else { "followUpMode" };
            if truthy(v(&[key])) { read(v(&[key])) } else { json!("one-at-a-time") }
        }
        "getThemeSetting" | "getTheme" => match defined(v(&["theme"])).and_then(Value::as_str) {
            Some(theme) if name == "getTheme" && theme.contains('/') => undefined(),
            Some(theme) => Value::from(theme),
            None => undefined(),
        },
        "getDefaultThinkingLevel" => read(v(&["defaultThinkingLevel"])),
        "getModelThinkingLevel" => {
            let key = format!("{}/{}", js_string(arg(0)), js_string(arg(1)));
            read(object(v(&["modelThinkingLevels"])).and_then(|map| map.get(&key)))
        }
        "getAllModelThinkingLevels" => Value::Object(spread(v(&["modelThinkingLevels"]))),
        "getTransport" => or(v(&["transport"]), json!("auto")),
        "getSessionShareDestination" => one_of(v(&["sessionShareDestination"]), &["gist", "radius"], "auto"),
        "getCompactionEnabled" => or(v(&["compaction", "enabled"]), json!(true)),
        "getCompactionReserveTokens" => compaction_tokens(merged, "reserveTokens", arg(0))?,
        "getCompactionKeepRecentTokens" => compaction_tokens(merged, "keepRecentTokens", arg(0))?,
        "getCompactionSettings" => compaction(merged, arg(0))?,
        "getBranchSummarySettings" => json!({
            "reserveTokens": or(v(&["branchSummary", "reserveTokens"]), json!(16384)),
            "skipPrompt": or(v(&["branchSummary", "skipPrompt"]), json!(false)),
        }),
        "getBranchSummarySkipPrompt" => or(v(&["branchSummary", "skipPrompt"]), json!(false)),
        "getRetryEnabled" => or(v(&["retry", "enabled"]), json!(true)),
        "getRetrySettings" => json!({
            "enabled": or(v(&["retry", "enabled"]), json!(true)),
            "maxRetries": or(v(&["retry", "maxRetries"]), json!(3)),
            "baseDelayMs": or(v(&["retry", "baseDelayMs"]), json!(2000)),
            "maxAgentDelayMs": or(v(&["retry", "maxAgentDelayMs"]), json!(60000)),
        }),
        "getHttpIdleTimeoutMs" => timeout(v(&["httpIdleTimeoutMs"]), "httpIdleTimeoutMs")?.unwrap_or(json!(300000)),
        "getCacheWarmingMode" => one_of(state.global.get("cacheWarming"), &["off", "streaming", "idle"], "streaming"),
        "getProviderRetrySettings" => json!({
            "timeoutMs": read(v(&["retry", "provider", "timeoutMs"])),
            "maxRetries": read(v(&["retry", "provider", "maxRetries"])),
            "maxRetryDelayMs": or(v(&["retry", "provider", "maxRetryDelayMs"]), json!(60000)),
        }),
        "getWebSocketConnectTimeoutMs" => {
            timeout(v(&["websocketConnectTimeoutMs"]), "websocketConnectTimeoutMs")?.unwrap_or_else(undefined)
        }
        "getHideThinkingBlock" => or(v(&["hideThinkingBlock"]), json!(false)),
        "getShowCacheMissNotices" => or(v(&["showCacheMissNotices"]), json!(false)),
        "getExternalEditorCommand" => editor(v(&["externalEditor"]), env),
        "getShellPath" => path_value(v(&["shellPath"]), env),
        "getQuietStartup" => match defined(v(&["quietStartup"])) {
            Some(value @ Value::Bool(true)) => value.clone(),
            Some(value) if value.as_str() == Some("header") => value.clone(),
            _ => json!(false),
        },
        "getDefaultProjectTrust" => one_of(state.global.get("defaultProjectTrust"), &["always", "never"], "ask"),
        "getShellCommandPrefix" => read(v(&["shellCommandPrefix"])),
        "getNpmCommand" if truthy(v(&["npmCommand"])) => array_copy(v(&["npmCommand"]))?,
        "getNpmCommand" => undefined(),
        "getCollapseChangelog" => or(v(&["collapseChangelog"]), json!(false)),
        "getEnableInstallTelemetry" => or(v(&["enableInstallTelemetry"]), json!(true)),
        "getEnableAnalytics" => or(v(&["enableAnalytics"]), json!(false)),
        "getTrackingId" => read(v(&["trackingId"])),
        "getPackages" => array_copy(v(&["packages"]))?,
        "getExtensionPaths" => array_copy(v(&["extensions"]))?,
        "getSkillPaths" => array_copy(v(&["skills"]))?,
        "getPromptTemplatePaths" => array_copy(v(&["prompts"]))?,
        "getThemePaths" => array_copy(v(&["themes"]))?,
        "getEnableSkillCommands" => or(v(&["enableSkillCommands"]), json!(true)),
        "getThinkingBudgets" => read(v(&["thinkingBudgets"])),
        "getTerminalCapabilityOverrides" => terminal_overrides(merged),
        "getShowImages" => or(v(&["terminal", "showImages"]), json!(true)),
        "getImageWidthCells" => match finite(v(&["terminal", "imageWidthCells"])) {
            Some(width) => number(width.floor().max(1.0)),
            None => json!(60),
        },
        "getClearOnShrink" => match defined(v(&["terminal", "clearOnShrink"])) {
            Some(value) => value.clone(),
            None => flag(env, "PI_CLEAR_ON_SHRINK"),
        },
        "getShowTerminalProgress" => or(v(&["terminal", "showTerminalProgress"]), json!(false)),
        "getTuiMode" => one_of(v(&["tuiMode"]), &["regular"], "fullscreen"),
        "getFullscreenExitOutput" => one_of(v(&["fullscreenExitOutput"]), &["resume-hint"], "transcript"),
        "getFullscreenScrollbar" => one_of(v(&["fullscreenScrollbar"]), &["always", "hidden"], "auto"),
        "getFullscreenCopyOnSelect" => or(v(&["fullscreenCopyOnSelect"]), json!(true)),
        "getFullscreenWheelScrollLines" => match finite(v(&["fullscreenWheelScrollLines"])) {
            Some(lines) => number(lines.floor().clamp(1.0, 100.0)),
            None => json!("auto"),
        },
        "getImageAutoResize" => or(v(&["images", "autoResize"]), json!(true)),
        "getBlockImages" => or(v(&["images", "blockImages"]), json!(false)),
        "getEnabledModels" => read(v(&["enabledModels"])),
        "getDefaultTools" => default_tools(v(&["defaultTools"])),
        "getDoubleEscapeAction" => or(v(&["doubleEscapeAction"]), json!("tree")),
        "getTreeFilterMode" => {
            one_of(v(&["treeFilterMode"]), &["default", "no-tools", "user-only", "labeled-only", "all"], "default")
        }
        "getShowHardwareCursor" if nullish(v(&["showHardwareCursor"])) => flag(env, "PI_HARDWARE_CURSOR"),
        "getShowHardwareCursor" => read(v(&["showHardwareCursor"])),
        "getEditorPaddingX" => or(v(&["editorPaddingX"]), json!(0)),
        "getOutputPad" => {
            let zero = matches!(v(&["outputPad"]), Some(Value::Number(pad)) if pad.as_f64() == Some(0.0));
            json!(if zero { 0 } else { 1 })
        }
        "getAutocompleteMaxVisible" => or(v(&["autocompleteMaxVisible"]), json!(5)),
        "getCodeBlockIndent" => or(v(&["markdown", "codeBlockIndent"]), json!("  ")),
        "getMermaidRenderingMode" => one_of(v(&["markdown", "mermaid"]), &["off", "final"], "streaming"),
        "getWarnings" => Value::Object(spread(v(&["warnings"]))),
        _ => return Ok(None),
    }))
}

impl SettingsManager {
    /// Run `name` with JavaScript `args` and environment `env`, returning the value and any alias
    /// token. Return `None` for an unknown member.
    pub(crate) fn sdk_call(
        &self,
        name: &str,
        args: &[Value],
        env: &Map<String, Value>,
    ) -> Result<Option<(Value, Option<Value>)>, String> {
        let args: Vec<Value> = args.iter().cloned().map(super::js::normalize).collect();
        let mut state = self.lock();
        if let Some(value) = getter(&state, name, &args, env)? {
            let alias = live_path(name, &args).and_then(|path| token(&state, &path));
            return Ok(Some((value, alias)));
        }
        Ok(super::sdk_setters::setter(&mut state, name, &args)?.map(|value| (value, None)))
    }
}
