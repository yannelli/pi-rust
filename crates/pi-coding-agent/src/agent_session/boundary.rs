//! Boundary entries from extensions (upstream `_applyBoundaryDrafts`, `_buildBoundaryContext`, and
//! `_commitBoundaryDrafts`): handlers of `turn_end` and `agent_before_settle` see the context their
//! drafts would produce, and the drafts are appended once all handlers ran.

use serde_json::{Value, json};

use super::{AgentSession, lock};
use crate::compaction::estimate_projected_context_tokens;
use crate::session::{FileEntry, NewCompaction, NewSessionOptions, SessionManager};

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
                let display = draft["display"].as_bool().unwrap_or(false);
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

impl AgentSession {
    /// A copy of the current branch with `drafts` applied.
    fn boundary_manager(&self, drafts: &[Value]) -> anyhow::Result<SessionManager> {
        let entries: Vec<FileEntry> = {
            let manager = self.session_manager();
            let mut entries = vec![manager.header_raw().clone()];
            entries.extend(manager.get_branch(None).into_iter().map(|entry| entry.raw().clone()));
            entries
        };
        let mut manager = SessionManager::in_memory_with(self.cwd(), NewSessionOptions::default(), entries)?;
        apply_drafts(&mut manager, drafts)?;
        Ok(manager)
    }

    /// `BoundaryContextPreview` for `drafts` (the `boundary.context` operation).
    pub(crate) fn boundary_preview(&self, kind: &str, drafts: &Value) -> Result<Value, String> {
        let drafts = drafts.as_array().cloned().unwrap_or_default();
        let manager = self.boundary_manager(&drafts).map_err(|error| format!("{error:#}"))?;
        let projection = manager.build_session_projection();
        let entries: Vec<Value> = projection
            .entries
            .iter()
            .map(|entry| json!({"sourceEntry": entry.source.raw(), "messages": entry.messages}))
            .collect();
        let llm = crate::messages::convert_to_llm(&projection.messages);
        let mut pending = self.agent.peek_queued_messages();
        pending.extend(lock(&self.state).pending_custom.iter().cloned());
        Ok(json!({
            "contextEntries": entries,
            "contextMessages": projection.messages,
            "llmMessages": llm,
            "pendingMessages": pending,
            "canContinue": self.can_continue_with(&llm, kind),
        }))
    }

    fn can_continue_with(&self, llm: &[pi_ai::Message], kind: &str) -> bool {
        let final_role = llm.last().map(pi_ai::Message::role);
        let has_context = llm.iter().any(|message| message.role() != "system");
        let pending_custom = !lock(&self.state).pending_custom.is_empty();
        let queued = self.agent.has_queued_messages();
        (has_context && final_role != Some("assistant"))
            || pending_custom
            || if kind == "turn_end" { queued } else { final_role == Some("assistant") && queued }
    }

    /// Whether the current context can run another request.
    pub(crate) fn boundary_can_continue(&self, kind: &str) -> bool {
        let messages = self.session_manager().build_session_projection().messages;
        self.can_continue_with(&crate::messages::convert_to_llm(&messages), kind)
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
