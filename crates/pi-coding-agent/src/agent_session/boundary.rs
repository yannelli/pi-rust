//! Boundary entries from extensions (upstream `_applyBoundaryDrafts`, `_buildBoundaryContext`, and
//! `_commitBoundaryDrafts`): handlers of `turn_end` and `agent_before_settle` see the context their
//! drafts would produce, and the drafts are appended once all handlers ran.
//!
//! pi-rust: the extension runtime builds that context from a plan of entry ids and its own copy of
//! the session entries, so a turn copies no messages, as upstream shares its objects.

use pi_ai::Message;
use serde_json::{Value, json};

use super::{AgentSession, lock};
use crate::compaction::estimate_projected_context_tokens;
use crate::messages::convert_message;
use crate::session::{Entry, EntryData, NewCompaction, SessionManager};

fn text(draft: &Value, key: &str) -> String {
    draft[key].as_str().unwrap_or_default().to_string()
}

fn optional(draft: &Value, key: &str) -> Option<Value> {
    draft.get(key).filter(|value| !value.is_null()).cloned()
}

/// Append `drafts` to `manager`; returns the new entry ids.
fn apply_drafts(manager: &mut SessionManager, drafts: &[Value]) -> anyhow::Result<Vec<String>> {
    let mut appended = Vec::new();
    for draft in drafts {
        let id = match draft["type"].as_str() {
            Some("custom") => manager.append_custom_entry(&text(draft, "customType"), optional(draft, "data"))?,
            Some("custom_message") => {
                let content = serde_json::from_value(draft["content"].clone())?;
                let display = draft["display"].as_bool();
                manager.append_custom_message_entry(
                    &text(draft, "customType"),
                    content,
                    display,
                    optional(draft, "details"),
                )?
            }
            Some("context_edit") => {
                let replacement =
                    optional(draft, "replacement").and_then(|replacement| optional(&replacement, "content"));
                manager.append_context_edit(&text(draft, "targetId"), replacement)?
            }
            Some("compaction") => {
                let tokens_before = {
                    let projection = manager.build_session_projection();
                    estimate_projected_context_tokens(&projection, &manager.get_branch(None)).tokens
                };
                manager.append_compaction_with(NewCompaction {
                    summary: text(draft, "summary"),
                    first_kept_entry_id: draft["firstKeptEntryId"].as_str().map(str::to_string),
                    tokens_before,
                    details: optional(draft, "details"),
                    from_hook: Some(true),
                    usage: optional(draft, "usage").and_then(|usage| serde_json::from_value(usage).ok()),
                })?
            }
            other => anyhow::bail!("Unknown boundary entry type: {}", other.unwrap_or("<missing>")),
        };
        appended.push(id);
    }
    Ok(appended)
}

/// What the model sees of a context (upstream `convertToLlm`) without copying it: the role of the
/// last message, and whether anything besides system messages is left.
#[derive(Default)]
struct LlmShape {
    final_role: Option<String>,
    has_context: bool,
}

impl LlmShape {
    /// Adds `message`; returns its conversion when it has a custom role, the only messages that
    /// change.
    fn add(&mut self, message: &Message) -> Option<Option<Message>> {
        let (role, converted) = match message {
            Message::Custom(_) => {
                let converted = convert_message(message);
                (converted.as_ref().map(|converted| converted.role().to_string()), Some(converted))
            }
            other => (Some(other.role().to_string()), None),
        };
        if let Some(role) = role {
            self.has_context |= role != "system";
            self.final_role = Some(role);
        }
        converted
    }
}

/// The messages of a projection in order; `None` stands for the entry's own message.
fn projected_messages<'a>(projection: &'a [(&'a Entry, Option<Vec<Message>>)]) -> impl Iterator<Item = &'a Message> {
    projection.iter().flat_map(|(entry, messages)| match (messages, entry.data()) {
        (Some(messages), _) => messages.as_slice(),
        (None, EntryData::Message(message)) => std::slice::from_ref(message),
        (None, _) => &[],
    })
}

/// The `boundary.plan` of `manager`'s context: each entry by id, with the messages it contributes
/// unless that is only its own message; the entries `added` (which the extension runtime does not
/// hold yet) whole; and the conversion of each custom-role message, as `[index, message]`.
fn context_plan(manager: &SessionManager, added: &[String]) -> (Value, LlmShape) {
    let projection = manager.build_shared_projection();
    let mut shape = LlmShape::default();
    let mut llm = Vec::new();
    for (index, message) in projected_messages(&projection).enumerate() {
        if let Some(converted) = shape.add(message) {
            llm.push(json!([index, converted]));
        }
    }
    let entries: Vec<Value> = projection
        .iter()
        .map(|(entry, messages)| match messages {
            Some(messages) => json!({"id": entry.id(), "messages": messages}),
            None => json!({"id": entry.id()}),
        })
        .collect();
    let sources: Vec<Value> = added
        .iter()
        .filter_map(|id| manager.get_entry(id))
        .map(|entry| serde_json::to_value(entry.raw()).unwrap_or(Value::Null))
        .collect();
    (json!({"entries": entries, "sources": sources, "llm": llm}), shape)
}

impl AgentSession {
    /// `BoundaryContextPreview` for `drafts` as a plan (the `boundary.plan` operation), with
    /// `pendingMessages` and `canContinue`. Upstream applies the drafts to a copy of the branch;
    /// here they stay in memory until the plan is built.
    pub(crate) fn boundary_plan(&self, kind: &str, drafts: &Value) -> Result<Value, String> {
        let drafts = drafts.as_array().cloned().unwrap_or_default();
        let (mut plan, shape) = {
            let mut manager = self.session_manager();
            let mut trial = manager.trial();
            let added = apply_drafts(&mut trial, &drafts).map_err(|error| format!("{error:#}"))?;
            context_plan(&trial, &added)
        };
        let mut pending = self.agent.peek_queued_messages();
        pending.extend(lock(&self.state).pending_custom.iter().cloned());
        plan["pendingMessages"] = json!(pending);
        plan["canContinue"] = json!(self.can_continue_with(&shape, kind));
        Ok(plan)
    }

    /// Fails when `drafts` do not apply to the current branch (the `boundary.check` operation);
    /// upstream finds out by building the context.
    pub(crate) fn boundary_check(&self, drafts: &Value) -> Result<(), String> {
        let drafts = drafts.as_array().cloned().unwrap_or_default();
        if drafts.is_empty() {
            return Ok(());
        }
        let applied = apply_drafts(&mut self.session_manager().trial(), &drafts);
        applied.map(drop).map_err(|error| format!("{error:#}"))
    }

    fn can_continue_with(&self, shape: &LlmShape, kind: &str) -> bool {
        let final_role = shape.final_role.as_deref();
        let pending_custom = !lock(&self.state).pending_custom.is_empty();
        let queued = self.agent.has_queued_messages();
        (shape.has_context && final_role != Some("assistant"))
            || pending_custom
            || if kind == "turn_end" { queued } else { final_role == Some("assistant") && queued }
    }

    /// Whether the current context can run another request.
    pub(crate) fn boundary_can_continue(&self, kind: &str) -> bool {
        let shape = {
            let manager = self.session_manager();
            let projection = manager.build_shared_projection();
            let mut shape = LlmShape::default();
            for message in projected_messages(&projection) {
                shape.add(message);
            }
            shape
        };
        self.can_continue_with(&shape, kind)
    }

    /// Append the drafts the boundary handlers settled on.
    pub(crate) fn commit_boundary_drafts(&self, drafts: &[Value]) {
        if drafts.is_empty() {
            return;
        }
        let appended = apply_drafts(&mut self.session_manager(), drafts);
        self.refresh_context();
        match appended {
            Ok(ids) => ids.iter().for_each(|id| self.emit_entry(id)),
            Err(error) => eprintln!("Warning: could not append boundary entries: {error:#}"),
        }
    }
}
