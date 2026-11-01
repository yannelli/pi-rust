//! Appending entries. Each entry becomes a child of the leaf and the new leaf. Field order
//! follows the upstream object literals so files match ones the TypeScript pi writes.

use pi_ai::{ContentBlock, Message, ThinkingLevel, Usage, UserContent};
use serde_json::{Value, json};

use super::SessionManager;
use super::entry::{Entry, EntryData};
use super::ids::{iso_timestamp, parse_iso};

/// Fields of a compaction entry. `first_kept_entry_id: None` keeps nothing before the entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewCompaction {
    pub summary: String,
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    pub details: Option<Value>,
    pub from_hook: Option<bool>,
    pub usage: Option<Usage>,
}

fn to_json<T: serde::Serialize>(value: &T) -> Option<Value> {
    serde_json::to_value(value).ok()
}

impl SessionManager {
    pub(crate) fn parent_value(&self) -> Value {
        self.leaf.clone().map_or(Value::Null, Value::String)
    }

    fn append_typed(&mut self, kind: &str, fields: Vec<(&str, Option<Value>)>) -> anyhow::Result<String> {
        let id = self.new_entry_id();
        let mut all = vec![
            ("type", Some(Value::String(kind.into()))),
            ("id", Some(Value::String(id))),
            ("parentId", Some(self.parent_value())),
            ("timestamp", Some(Value::String(iso_timestamp()))),
        ];
        all.extend(fields);
        self.append_entry(Entry::build(all))
    }

    /// Append a message. Summary roles belong in compaction and branch summary entries instead.
    pub fn append_message(&mut self, message: &Message) -> anyhow::Result<String> {
        self.append_typed("message", vec![("message", to_json(message))])
    }

    pub fn append_thinking_level_change(&mut self, level: ThinkingLevel) -> anyhow::Result<String> {
        self.append_typed("thinking_level_change", vec![("thinkingLevel", Some(json!(level.as_str())))])
    }

    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> anyhow::Result<String> {
        self.append_typed("model_change", vec![("provider", Some(json!(provider))), ("modelId", Some(json!(model_id)))])
    }

    /// Model-attributed usage outside the LLM context, such as `cache_warm`.
    pub fn append_usage(
        &mut self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: &Usage,
        note: Option<&str>,
    ) -> anyhow::Result<String> {
        self.append_usage_json(kind, provider, model, Some(serde_json::to_value(usage)?), note)
    }

    /// `append_usage` with the usage JSON stored as given, as upstream `appendUsage` does: `None` (an
    /// `undefined` usage) leaves the key out, as `JSON.stringify` does, and `Some(Null)` stores `null`.
    pub(crate) fn append_usage_json(
        &mut self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: Option<Value>,
        note: Option<&str>,
    ) -> anyhow::Result<String> {
        self.append_typed(
            "usage",
            vec![
                ("kind", Some(json!(kind))),
                ("provider", Some(json!(provider))),
                ("model", Some(json!(model))),
                ("usage", usage),
                ("note", note.filter(|note| !note.is_empty()).map(|note| json!(note))),
            ],
        )
    }

    /// A compaction with only the required fields.
    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: &str,
        tokens_before: u64,
    ) -> anyhow::Result<String> {
        self.append_compaction_with(NewCompaction {
            summary: summary.to_string(),
            first_kept_entry_id: Some(first_kept_entry_id.to_string()),
            tokens_before,
            ..NewCompaction::default()
        })
    }

    /// A compaction entry with its system checkpoint: the replayed prompt and tools at this point.
    pub fn append_compaction_with(&mut self, compaction: NewCompaction) -> anyhow::Result<String> {
        let timestamp = iso_timestamp();
        let system =
            pi_ai::transcript::current_system_message(&self.build_session_projection().messages).map(|mut system| {
                system.timestamp = parse_iso(&timestamp).unwrap_or_default();
                system
            });
        let id = self.new_entry_id();
        let entry = Entry::build(vec![
            ("type", Some(json!("compaction"))),
            ("id", Some(json!(id))),
            ("parentId", Some(self.parent_value())),
            ("timestamp", Some(json!(timestamp))),
            ("summary", Some(json!(compaction.summary))),
            ("firstKeptEntryId", Some(json!(compaction.first_kept_entry_id.unwrap_or_else(|| id.clone())))),
            ("tokensBefore", Some(json!(compaction.tokens_before))),
            ("details", compaction.details),
            ("usage", compaction.usage.as_ref().and_then(to_json)),
            ("fromHook", compaction.from_hook.map(Value::Bool)),
            ("systemMessage", system.as_ref().and_then(to_json)),
        ]);
        self.append_entry(entry)
    }

    /// Extension state that never enters the LLM context.
    pub fn append_custom_entry(&mut self, custom_type: &str, data: Option<Value>) -> anyhow::Result<String> {
        let entry = Entry::build(vec![
            ("type", Some(json!("custom"))),
            ("customType", Some(json!(custom_type))),
            ("data", data),
            ("id", Some(json!(self.new_entry_id()))),
            ("parentId", Some(self.parent_value())),
            ("timestamp", Some(json!(iso_timestamp()))),
        ]);
        self.append_entry(entry)
    }

    /// Set the display name. Line breaks become spaces; an empty name clears it.
    pub fn append_session_info(&mut self, name: &str) -> anyhow::Result<String> {
        let mut sanitized = String::new();
        let mut in_break = false;
        for ch in name.chars() {
            if ch == '\r' || ch == '\n' {
                if !in_break {
                    sanitized.push(' ');
                }
                in_break = true;
            } else {
                sanitized.push(ch);
                in_break = false;
            }
        }
        self.append_typed("session_info", vec![("name", Some(json!(sanitized.trim())))])
    }

    /// An extension message that enters the LLM context as a `custom` role message.
    pub fn append_custom_message_entry(
        &mut self,
        custom_type: &str,
        content: UserContent,
        display: bool,
        details: Option<Value>,
    ) -> anyhow::Result<String> {
        let entry = Entry::build(vec![
            ("type", Some(json!("custom_message"))),
            ("customType", Some(json!(custom_type))),
            ("content", to_json(&content)),
            ("display", Some(json!(display))),
            ("details", details),
            ("id", Some(json!(self.new_entry_id()))),
            ("parentId", Some(self.parent_value())),
            ("timestamp", Some(json!(iso_timestamp()))),
        ]);
        self.append_entry(entry)
    }

    /// Edit an earlier entry's contribution to future model context on this branch. `None`
    /// omits the target; `Some(content)` (string or block array) replaces only its content.
    pub fn append_context_edit(&mut self, target_id: &str, replacement: Option<Value>) -> anyhow::Result<String> {
        if replacement.as_ref().is_some_and(|content| !content.is_string() && !content.is_array()) {
            anyhow::bail!("Context edit replacement must be null or contain string/array content");
        }
        let Some(target) = self.get_entry(target_id) else { anyhow::bail!("Entry {target_id} not found") };
        if !self.get_branch(None).iter().any(|entry| entry.id() == target_id) {
            anyhow::bail!("Entry {target_id} is not on the active branch");
        }
        let role = match target.data() {
            EntryData::CustomMessage(_) => "custom",
            EntryData::Message(message) if matches!(message.role(), "user" | "assistant" | "toolResult") => {
                message.role()
            }
            _ => anyhow::bail!("Entry {target_id} does not contribute editable model content"),
        };
        let replacement = match replacement {
            Some(Value::String(text)) if matches!(role, "assistant" | "toolResult") => {
                json!({ "content": [ContentBlock::text(text)] })
            }
            Some(content) => json!({ "content": content }),
            None => Value::Null,
        };
        self.append_typed(
            "context_edit",
            vec![("targetId", Some(json!(target_id))), ("replacement", Some(replacement))],
        )
    }

    /// Set (or clear with `None` or an empty label) a bookmark on an entry.
    pub fn append_label_change(&mut self, target_id: &str, label: Option<&str>) -> anyhow::Result<String> {
        if self.get_entry(target_id).is_none() {
            anyhow::bail!("Entry {target_id} not found");
        }
        let label_value = label.map(|label| json!(label));
        let id = self.append_typed("label", vec![("targetId", Some(json!(target_id))), ("label", label_value)])?;
        let timestamp = self.get_entry(&id).map(|entry| entry.timestamp().to_string()).unwrap_or_default();
        match label.filter(|label| !label.is_empty()) {
            Some(label) => {
                self.labels.insert(target_id.to_string(), (label.to_string(), timestamp));
            }
            None => {
                self.labels.remove(target_id);
            }
        }
        Ok(id)
    }

    /// Move the leaf to `branch_from` (`None` for a new root) and record a summary of the
    /// abandoned path there. `fromId` is the leaf before the move, or `root`.
    pub fn branch_with_summary(
        &mut self,
        branch_from: Option<&str>,
        summary: &str,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> anyhow::Result<String> {
        if let Some(id) = branch_from
            && self.get_entry(id).is_none()
        {
            anyhow::bail!("Entry {id} not found");
        }
        let from_id = self.leaf.clone().unwrap_or_else(|| "root".into());
        self.leaf = branch_from.map(String::from);
        self.append_typed(
            "branch_summary",
            vec![
                ("fromId", Some(json!(from_id))),
                ("summary", Some(json!(summary))),
                ("details", details),
                ("usage", usage.as_ref().and_then(to_json)),
                ("fromHook", from_hook.map(Value::Bool)),
            ],
        )
    }
}
