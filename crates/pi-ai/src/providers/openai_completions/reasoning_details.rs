//! OpenRouter-style `reasoning_details`: validated, merged while streaming,
//! stored as the thinking signature, and replayed on later turns.

use serde_json::Value;

/// Whether `detail` is a well-formed `reasoning_details` entry.
pub(super) fn is_reasoning_detail(detail: &Value) -> bool {
    let Some(object) = detail.as_object() else { return false };
    let optional = |key: &str, check: fn(&Value) -> bool| object.get(key).is_none_or(check);
    let common = optional("id", |value| value.is_null() || value.is_string())
        && optional("format", Value::is_string)
        && optional("index", Value::is_number);
    common
        && match object.get("type").and_then(Value::as_str) {
            Some("reasoning.summary") => object.get("summary").is_some_and(Value::is_string),
            Some("reasoning.encrypted") => object.get("data").is_some_and(Value::is_string),
            Some("reasoning.text") => {
                object.get("text").is_some_and(Value::is_string)
                    && optional("signature", |value| value.is_null() || value.is_string())
            }
            _ => false,
        }
}

/// Append a streamed detail, merging consecutive text or summary deltas.
pub(super) fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let kind = detail["type"].as_str().unwrap_or_default();
    let field = match kind {
        "reasoning.text" => Some("text"),
        "reasoning.summary" => Some("summary"),
        _ => None,
    };
    if let (Some(field), Some(last)) = (field, details.last_mut())
        && last["type"] == kind
    {
        let addition = detail[field].as_str().unwrap_or_default();
        let merged = format!("{}{addition}", last[field].as_str().unwrap_or_default());
        last[field] = Value::String(merged);
        // `signature` and `format` fill when empty, `id` and `index` only when absent.
        let empty = |value: &Value| value.is_null() || value.as_str() == Some("");
        let keys: &[(&str, bool)] = &[("signature", true), ("id", false), ("format", true), ("index", false)];
        for (key, fill_empty) in keys {
            let missing = if *fill_empty { empty(&last[*key]) } else { last[*key].is_null() };
            if (*key != "signature" || kind == "reasoning.text") && missing && !detail[*key].is_null() {
                last[*key] = detail[*key].clone();
            }
        }
        return;
    }
    details.push(detail.clone());
}

pub(super) fn parse_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    let details = parsed.as_array()?;
    (!details.is_empty() && details.iter().all(is_reasoning_detail)).then(|| details.clone())
}

/// Older sessions stored one encrypted detail in a tool call's `thoughtSignature`.
pub(super) fn parse_legacy_encrypted_detail(signature: Option<&str>) -> Option<Value> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    let valid = is_reasoning_detail(&parsed)
        && parsed["type"] == "reasoning.encrypted"
        && parsed["id"].as_str().is_some_and(|id| !id.is_empty())
        && parsed["data"].as_str().is_some_and(|data| !data.is_empty());
    valid.then_some(parsed)
}
