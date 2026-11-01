//! Reads the SDK getters share (upstream `settings-manager.ts` helpers): optional chaining, token
//! and timeout validation, `normalizePath`, the editor fallback and `defaultTools` resolution.

use serde_json::{Map, Value, json};

use super::js::{as_number, defined, js_string, object, read, to_number, truthy, undefined};
use super::tools::resolve_default_tools;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `map.a?.b?.c`.
pub(super) fn at<'a>(map: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    rest.iter().try_fold(map.get(*first)?, |value, key| object(Some(value))?.get(*key))
}

/// `value` when it is one of `options`, else `fallback`.
pub(super) fn one_of(value: Option<&Value>, options: &[&str], fallback: &str) -> Value {
    let found = defined(value).and_then(Value::as_str).filter(|text| options.contains(text));
    Value::from(found.unwrap_or(fallback))
}

/// `[...(value ?? [])]`.
pub(super) fn array_copy(value: Option<&Value>) -> Result<Value, String> {
    match defined(value) {
        None | Some(Value::Null) => Ok(json!([])),
        Some(Value::Array(items)) => Ok(Value::Array(items.clone())),
        Some(Value::String(text)) => Ok(text.chars().map(|c| Value::from(c.to_string())).collect()),
        Some(other) => Err(format!("{} is not iterable", js_string(Some(other)))),
    }
}

fn safe_count(value: &Value) -> bool {
    matches!(value, Value::Number(_))
        && as_number(value).is_some_and(|n| n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER && n >= 0.0)
}

/// Upstream `getCompactionTokenSetting`: model override, then the ordinary setting, then the default.
pub(super) fn compaction_tokens(
    merged: &Map<String, Value>,
    field: &str,
    model: Option<&Value>,
) -> Result<Value, String> {
    let invalid = "Expected a non-negative safe integer.";
    let ordinary = defined(at(merged, &["compaction", field]));
    if let Some(value) = ordinary.filter(|value| !safe_count(value)) {
        return Err(format!("Invalid compaction.{field} setting: {}. {invalid}", js_string(Some(value))));
    }
    let key = truthy(model).then(|| {
        let part = |name: &str| js_string(model.and_then(|model| object(Some(model))?.get(name)));
        format!("{}/{}", part("provider"), part("id"))
    });
    let overrides = object(at(merged, &["compaction", "modelOverrides"]));
    let entry = key.as_ref().and_then(|key| defined(overrides?.get(key)));
    let key = key.unwrap_or_default();
    if let Some(entry) = entry.filter(|entry| object(Some(entry)).is_none()) {
        let shown = js_string(Some(entry));
        return Err(format!("Invalid compaction.modelOverrides[\"{key}\"] setting: {shown}. Expected an object."));
    }
    let override_value = defined(entry.and_then(|entry| object(Some(entry))?.get(field)));
    if let Some(value) = override_value.filter(|value| !safe_count(value)) {
        let shown = js_string(Some(value));
        return Err(format!("Invalid compaction.modelOverrides[\"{key}\"].{field} setting: {shown}. {invalid}"));
    }
    let fallback = if field == "reserveTokens" { 16384 } else { 20000 };
    Ok(override_value.or(ordinary).cloned().unwrap_or_else(|| json!(fallback)))
}

/// Upstream `parseHttpIdleTimeoutMs`.
fn parse_timeout(value: Option<&Value>) -> Option<f64> {
    let milliseconds = match defined(value)? {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0.0);
            }
            if trimmed.is_empty() {
                return None;
            }
            to_number(Some(&Value::from(trimmed)))
        }
        other => as_number(other)?,
    };
    (milliseconds.is_finite() && milliseconds >= 0.0).then(|| milliseconds.floor())
}

/// Upstream `parseTimeoutSetting`: the parsed value, `None` when unset, and an error otherwise.
pub(super) fn timeout(value: Option<&Value>, name: &str) -> Result<Option<Value>, String> {
    match parse_timeout(value) {
        Some(milliseconds) => Ok(Some(super::js::number(milliseconds))),
        None if defined(value).is_some() => Err(format!("Invalid {name} setting: {}", js_string(value))),
        None => Ok(None),
    }
}

/// A non-empty environment variable from the map JavaScript passed.
pub(super) fn env<'a>(env: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    env.get(key).and_then(Value::as_str).filter(|value| !value.is_empty())
}

/// `path.posix.join(home, rest)`.
fn join(home: &str, rest: &str) -> String {
    let joined = format!("{home}/{rest}");
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut out = format!("{}{}", if joined.starts_with('/') { "/" } else { "" }, parts.join("/"));
    if joined.ends_with('/') && !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let pair = bytes.get(index + 1..index + 3).and_then(|pair| std::str::from_utf8(pair).ok());
        match pair.and_then(|pair| u8::from_str_radix(pair, 16).ok()).filter(|_| bytes[index] == b'%') {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Upstream `normalizePath` with its defaults: `~` expands, `file://` URLs become paths.
fn normalize_path(path: &str, env_map: &Map<String, Value>) -> String {
    let home = env(env_map, "HOME").map(String::from).or_else(|| std::env::var("HOME").ok()).unwrap_or_default();
    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return join(&home, rest);
    }
    match path.strip_prefix("file://") {
        Some(rest) => percent_decode(rest.strip_prefix("localhost").unwrap_or(rest)),
        None => path.to_string(),
    }
}

/// `value ? normalizePath(value) : value`.
pub(super) fn path_value(value: Option<&Value>, env_map: &Map<String, Value>) -> Value {
    match defined(value) {
        Some(Value::String(path)) if !path.is_empty() => Value::from(normalize_path(path, env_map)),
        _ => read(value),
    }
}

/// Upstream `getExternalEditorCommand`.
pub(super) fn editor(configured: Option<&Value>, env_map: &Map<String, Value>) -> Value {
    if let Some(command) = defined(configured).and_then(Value::as_str).filter(|command| !command.trim().is_empty()) {
        return Value::from(command);
    }
    let fallback = if cfg!(windows) { "notepad" } else { "nano" };
    Value::from(env(env_map, "VISUAL").or_else(|| env(env_map, "EDITOR")).unwrap_or(fallback))
}

/// Upstream `getTerminalCapabilityOverrides`.
pub(super) fn terminal_overrides(merged: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    match defined(at(merged, &["terminal", "images"])) {
        Some(Value::String(images)) if images == "kitty" || images == "iterm2" => {
            out.insert("images".into(), Value::from(images.as_str()));
        }
        Some(Value::Bool(false)) => {
            out.insert("images".into(), Value::Null);
        }
        _ => {}
    }
    for key in ["trueColor", "hyperlinks"] {
        if let Some(flag @ Value::Bool(_)) = at(merged, &["terminal", key]) {
            out.insert(key.into(), flag.clone());
        }
    }
    Value::Object(out)
}

/// Upstream `getDefaultTools`: `undefined` when unset, else the resolved selection.
pub(super) fn default_tools(value: Option<&Value>) -> Value {
    let Some(tools) = defined(value) else { return undefined() };
    let names: Vec<String> = match tools {
        Value::Array(items) => items.iter().filter_map(|item| item.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    json!(resolve_default_tools(&names))
}
