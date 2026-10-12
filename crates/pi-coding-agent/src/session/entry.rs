//! Session file lines. Each [`Entry`] keeps its JSON object exactly as stored, so files
//! written by the TypeScript pi round-trip unchanged, plus a typed view of its fields.

use pi_ai::{Message, SystemMessage, Usage, UserContent};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// One raw JSON line of a session file (header or entry), in stored key order.
pub type FileEntry = Map<String, Value>;

pub const CURRENT_SESSION_VERSION: u32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    /// Absent in version 1 files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub id: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

pub(crate) fn lenient_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|value| value.as_u64().or_else(|| value.as_f64().map(|float| float.max(0.0) as u64)))
        .unwrap_or(0))
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEntry {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, deserialize_with = "lenient_usage")]
    pub usage: Usage,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub first_kept_entry_id: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub tokens_before: u64,
    #[serde(default)]
    pub details: Option<Value>,
    #[serde(default, deserialize_with = "lenient_optional_usage")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub from_hook: Option<bool>,
    #[serde(default, deserialize_with = "lenient_system_message")]
    pub system_message: Option<SystemMessage>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(default)]
    pub from_id: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub details: Option<Value>,
    #[serde(default, deserialize_with = "lenient_optional_usage")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub from_hook: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageEntry {
    #[serde(default)]
    pub custom_type: String,
    #[serde(default = "empty_user_content", deserialize_with = "lenient_content")]
    pub content: UserContent,
    #[serde(default)]
    pub details: Option<Value>,
    #[serde(default)]
    pub display: bool,
}

/// Content of a context edit replacement: a string or an array of content blocks.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ContextReplacement {
    #[serde(default)]
    pub content: Value,
}

/// Typed fields of an entry. `Other` covers unknown types, which are kept and ignored.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EntryData {
    Message(Message),
    ThinkingLevelChange { thinking_level: String },
    ModelChange { provider: String, model_id: String },
    Usage(UsageEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    Custom { custom_type: String, data: Option<Value> },
    CustomMessage(CustomMessageEntry),
    ContextEdit { target_id: String, replacement: Option<ContextReplacement> },
    Label { target_id: String, label: Option<String> },
    SessionInfo { name: Option<String> },
    Other,
}

fn lenient_usage<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Usage, D::Error> {
    Ok(lenient_optional_usage(deserializer)?.unwrap_or_default())
}

fn lenient_optional_usage<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Usage>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.filter(Value::is_object).map(|mut value| {
        normalize_usage(&mut value);
        serde_json::from_value(value).unwrap_or_default()
    }))
}

fn lenient_system_message<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<SystemMessage>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|mut value| {
        if let Some(object) = value.as_object_mut() {
            object.entry("content").or_insert(Value::String(String::new()));
            object.entry("timestamp").or_insert(Value::from(0));
        }
        serde_json::from_value(value).ok()
    }))
}

fn empty_user_content() -> UserContent {
    UserContent::Blocks(Vec::new())
}

fn lenient_content<'de, D: Deserializer<'de>>(deserializer: D) -> Result<UserContent, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()).unwrap_or(UserContent::Blocks(Vec::new())))
}

fn normalize_usage(value: &mut Value) {
    let Some(usage) = value.as_object_mut() else { return };
    for key in ["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
        let number = usage.get(key).and_then(|value| value.as_f64()).unwrap_or(0.0).max(0.0) as u64;
        usage.insert(key.into(), Value::from(number));
    }
    let cost = usage.entry("cost").or_insert_with(|| Value::Object(Map::new()));
    if !cost.is_object() {
        *cost = Value::Object(Map::new());
    }
    if let Some(cost) = cost.as_object_mut() {
        for key in ["input", "output", "cacheRead", "cacheWrite", "total"] {
            let number = cost.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            cost.insert(key.into(), Value::from(number));
        }
    }
}

/// Parse a stored message, filling fields that older or hand-edited files may lack,
/// as upstream `sessionEntryToContextMessages` tolerates missing content.
pub fn parse_message_lenient(value: &Value) -> Option<Message> {
    if let Ok(message) = serde_json::from_value::<Message>(value.clone()) {
        return Some(message);
    }
    let mut object = value.as_object()?.clone();
    let role = object.get("role")?.as_str()?.to_string();
    let empty_content = if role == "system" { Value::String(String::new()) } else { Value::Array(Vec::new()) };
    if object.get("content").is_none_or(Value::is_null) {
        object.insert("content".into(), empty_content);
    }
    if !object.get("timestamp").is_some_and(Value::is_number) {
        object.insert("timestamp".into(), Value::from(0));
    }
    match role.as_str() {
        "assistant" => {
            for key in ["api", "provider", "model"] {
                object.entry(key).or_insert(Value::String(String::new()));
            }
            object.entry("stopReason").or_insert(Value::String("stop".into()));
            let usage = object.entry("usage").or_insert_with(|| Value::Object(Map::new()));
            normalize_usage(usage);
        }
        "toolResult" => {
            for key in ["toolCallId", "toolName"] {
                object.entry(key).or_insert(Value::String(String::new()));
            }
            object.entry("isError").or_insert(Value::Bool(false));
            if let Some(usage) = object.get_mut("usage") {
                normalize_usage(usage);
            }
        }
        _ => {}
    }
    serde_json::from_value(Value::Object(object)).ok()
}

/// One session entry: base fields, a typed view, and the stored JSON object.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    kind: String,
    id: String,
    parent_id: Option<String>,
    timestamp: String,
    data: EntryData,
    raw: FileEntry,
}

fn string_field(raw: &FileEntry, key: &str) -> Option<String> {
    raw.get(key).and_then(Value::as_str).map(String::from)
}

impl Entry {
    /// Wrap a stored entry object. Objects without a string `type` get an empty kind.
    pub fn from_raw(raw: FileEntry) -> Self {
        let kind = string_field(&raw, "type").unwrap_or_default();
        let data = parse_data(&kind, &raw);
        Self {
            id: string_field(&raw, "id").unwrap_or_default(),
            parent_id: string_field(&raw, "parentId"),
            timestamp: string_field(&raw, "timestamp").unwrap_or_default(),
            kind,
            data,
            raw,
        }
    }

    /// Build an entry from `(key, value)` pairs in write order; `None` values are left out.
    pub(crate) fn build(fields: Vec<(&str, Option<Value>)>) -> Self {
        Self::from_raw(
            fields.into_iter().filter_map(|(key, value)| value.map(|value| (key.to_string(), value))).collect(),
        )
    }

    /// The entry `type`, such as `message` or `compaction`.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }

    /// ISO 8601 entry timestamp.
    pub fn timestamp(&self) -> &str {
        &self.timestamp
    }

    pub fn data(&self) -> &EntryData {
        &self.data
    }

    /// The stored JSON object, in file key order.
    pub fn raw(&self) -> &FileEntry {
        &self.raw
    }

    pub fn into_raw(self) -> FileEntry {
        self.raw
    }

    /// The stored message of a `message` entry.
    pub fn message(&self) -> Option<&Message> {
        match &self.data {
            EntryData::Message(message) => Some(message),
            _ => None,
        }
    }

    pub fn compaction(&self) -> Option<&CompactionEntry> {
        match &self.data {
            EntryData::Compaction(compaction) => Some(compaction),
            _ => None,
        }
    }

    pub fn branch_summary(&self) -> Option<&BranchSummaryEntry> {
        match &self.data {
            EntryData::BranchSummary(summary) => Some(summary),
            _ => None,
        }
    }

    /// Whether this is a message entry whose message has `role`.
    pub fn is_message_role(&self, role: &str) -> bool {
        self.message().is_some_and(|message| message.role() == role)
    }

    /// A copy with `key` set (or removed for `None`), keeping its position in the object.
    pub(crate) fn with_field(&self, key: &str, value: Option<Value>) -> Self {
        let mut raw = self.raw.clone();
        match value {
            Some(value) => {
                raw.insert(key.to_string(), value);
            }
            None => {
                raw.shift_remove(key);
            }
        }
        Self::from_raw(raw)
    }

    pub(crate) fn with_parent(&self, parent_id: Option<&str>) -> Self {
        self.with_field("parentId", Some(parent_id.map_or(Value::Null, |id| Value::String(id.into()))))
    }

    pub fn to_json_line(&self) -> String {
        serde_json::to_string(&self.raw).unwrap_or_default()
    }
}

fn parse_data(kind: &str, raw: &FileEntry) -> EntryData {
    let object = Value::Object(raw.clone());
    let parse = |value: &Value| -> Option<EntryData> {
        Some(match kind {
            "message" => EntryData::Message(parse_message_lenient(raw.get("message")?)?),
            "thinking_level_change" => {
                EntryData::ThinkingLevelChange { thinking_level: string_field(raw, "thinkingLevel")? }
            }
            "model_change" => EntryData::ModelChange {
                provider: string_field(raw, "provider")?,
                model_id: string_field(raw, "modelId")?,
            },
            "usage" => EntryData::Usage(serde_json::from_value(value.clone()).ok()?),
            "compaction" => EntryData::Compaction(serde_json::from_value(value.clone()).ok()?),
            "branch_summary" => EntryData::BranchSummary(serde_json::from_value(value.clone()).ok()?),
            "custom" => EntryData::Custom {
                custom_type: string_field(raw, "customType").unwrap_or_default(),
                data: raw.get("data").cloned(),
            },
            "custom_message" => EntryData::CustomMessage(serde_json::from_value(value.clone()).ok()?),
            "context_edit" => EntryData::ContextEdit {
                target_id: string_field(raw, "targetId")?,
                replacement: raw.get("replacement").and_then(|value| serde_json::from_value(value.clone()).ok()),
            },
            "label" => {
                EntryData::Label { target_id: string_field(raw, "targetId")?, label: string_field(raw, "label") }
            }
            "session_info" => EntryData::SessionInfo { name: string_field(raw, "name") },
            _ => return None,
        })
    };
    parse(&object).unwrap_or(EntryData::Other)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn keeps_raw_objects_and_parses_typed_views() {
        let line = r#"{"type":"custom","customType":"ext","data":{"n":1},"id":"a1","parentId":null,"timestamp":"t"}"#;
        let entry = Entry::from_raw(serde_json::from_str(line).unwrap());
        assert_eq!(entry.to_json_line(), line);
        assert_eq!((entry.kind(), entry.id(), entry.parent_id()), ("custom", "a1", None));
        assert!(matches!(entry.data(), EntryData::Custom { custom_type, .. } if custom_type == "ext"));
        let moved = entry.with_parent(Some("p"));
        assert_eq!(moved.to_json_line(), line.replace("null", "\"p\""));
    }

    #[test]
    fn tolerates_legacy_messages() {
        let value = json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}], "api": "test",
            "provider": "test", "model": "test", "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
            "stopReason": "stop", "timestamp": 2});
        let Some(Message::Assistant(message)) = parse_message_lenient(&value) else { panic!("assistant") };
        assert_eq!(message.usage.input, 1);
        let user = parse_message_lenient(&json!({"role": "user", "content": null})).unwrap();
        assert!(matches!(user, Message::User(user) if user.content.text().is_empty()));
    }
}
