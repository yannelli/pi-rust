//! What a streamed answer changed in its partial message between two partial commits, so a commit costs the new text
//! (upstream `harness/partial-changes.ts`).

use std::collections::{BTreeMap, HashMap};

use pi_ai::{AssistantMessageEvent, JsonChange, PathSegment, StreamingJsonParser};
use pi_chord::delta::{Change, Seg};
use pi_chord::{ChordResult, Json, JsonMap};

use super::json::{AssignPlan, assign_json, node_mut};

/// What one partial commit writes besides the partial message itself; from [`PartialChanges::take`].
#[derive(Clone, Debug)]
pub(crate) struct PartialCommit {
    /// The plan `assign_json()` writes the message with: text appends, and the arguments it leaves to the changes.
    plan: AssignPlan,
    /// Each tracked tool call by content index: its argument changes since the last commit, and its snapshot.
    tool_calls: BTreeMap<usize, (Vec<JsonChange>, Json)>,
}

/// What a tracked tool call changed since the last commit, and whether any commit took its changes yet.
struct ToolCallState {
    parser: StreamingJsonParser,
    changes: Vec<JsonChange>,
    committed: bool,
}

/// Collects what a response's events changed in its partial message since the last commit: text and thinking deltas
/// to append (dropped when their block ends), and per tool call the changes its own `StreamingJsonParser` reports for
/// the raw argument text. A commit then appends the new text instead of assigning strings that Chord compares whole.
#[derive(Default)]
pub(crate) struct PartialChanges {
    text: BTreeMap<usize, (&'static str, String)>,
    tool_calls: HashMap<usize, ToolCallState>,
}

fn path_of(change: &JsonChange) -> &[PathSegment] {
    match change {
        JsonChange::Set { path, .. } | JsonChange::Append { path, .. } => path,
    }
}

impl PartialChanges {
    /// Record what `event` changed.
    pub fn observe(&mut self, event: &AssistantMessageEvent) {
        match event {
            AssistantMessageEvent::TextDelta { content_index, delta, .. }
            | AssistantMessageEvent::ThinkingDelta { content_index, delta, .. } => {
                let field = if matches!(event, AssistantMessageEvent::TextDelta { .. }) { "text" } else { "thinking" };
                match self.text.get_mut(content_index) {
                    Some((pending, text)) if *pending == field => text.push_str(delta),
                    _ => {
                        self.text.insert(*content_index, (field, delta.clone()));
                    }
                }
            }
            AssistantMessageEvent::ToolcallStart { content_index, .. } => {
                let call = ToolCallState { parser: StreamingJsonParser::new(), changes: Vec::new(), committed: false };
                self.tool_calls.insert(*content_index, call);
            }
            AssistantMessageEvent::ToolcallDelta { content_index, delta, .. } => {
                let Some(call) = self.tool_calls.get_mut(content_index) else { return };
                for change in call.parser.append(delta) {
                    // Arguments are an object: any other root shows as no arguments, so the snapshot is committed.
                    let object_root = matches!(change, JsonChange::Set { value: serde_json::Value::Object(_), .. });
                    if path_of(change).is_empty() && !object_root {
                        self.tool_calls.remove(content_index);
                        return;
                    }
                    call.changes.push(change.clone());
                }
            }
            AssistantMessageEvent::TextEnd { content_index, .. }
            | AssistantMessageEvent::ThinkingEnd { content_index, .. } => {
                self.text.remove(content_index);
            }
            AssistantMessageEvent::ToolcallEnd { content_index, .. } => {
                self.tool_calls.remove(content_index);
            }
            _ => {}
        }
    }

    /// Take what changed since the last call, for a commit of `message`, a copy of the partial. Clears the arguments
    /// of tracked tool calls in `message`: the commit writes them from the changes. Call `restore()` if it fails.
    pub fn take(&mut self, message: &mut Json) -> PartialCommit {
        let mut blocks = HashMap::new();
        for (index, (field, text)) in std::mem::take(&mut self.text) {
            let fields = HashMap::from([(Seg::key(field), AssignPlan::Append(text))]);
            blocks.insert(Seg::Index(index as u64), AssignPlan::Fields(fields));
        }
        let mut tool_calls = BTreeMap::new();
        let mut content = message.get_mut("content").and_then(Json::as_array_mut);
        for (index, call) in &mut self.tool_calls {
            let block = content.as_deref_mut().and_then(|content| content.get_mut(*index));
            let Some(block) = block.and_then(Json::as_object_mut) else { continue };
            let snapshot = block.insert("arguments", Json::object(JsonMap::new())).unwrap_or_default();
            tool_calls.insert(*index, (std::mem::take(&mut call.changes), snapshot));
            let fields = HashMap::from([(Seg::key("arguments"), AssignPlan::Keep)]);
            blocks.insert(Seg::Index(*index as u64), AssignPlan::Fields(fields));
        }
        let plan = AssignPlan::Fields(HashMap::from([(Seg::key("content"), AssignPlan::Fields(blocks))]));
        PartialCommit { plan, tool_calls }
    }

    /// Write `message` with what `commit` took into the `message` member of the object at `generation`: the message
    /// leaf by leaf with the plan, then the argument changes. A call whose block is new here starts from its first
    /// piece; one whose committed changes another writer dropped from the message falls back to its snapshot.
    pub fn commit(
        &mut self,
        change: &mut Change,
        generation: &[Seg],
        mut message: Json,
        commit: &PartialCommit,
    ) -> ChordResult<()> {
        let mut path = generation.to_vec();
        path.push(Seg::key("message"));
        let committed_blocks = node_mut(change.state()?, &path)
            .and_then(|message| message.get("content"))
            .and_then(Json::as_array)
            .map_or(0, Vec::len);
        let mut apply = Vec::new();
        for (index, (changes, snapshot)) in &commit.tool_calls {
            let Some(call) = self.tool_calls.get_mut(index) else { continue };
            if *index >= committed_blocks && call.committed {
                // The earlier changes are not in the committed message: commit the snapshot and stop tracking.
                self.tool_calls.remove(index);
                let content = message.get_mut("content").and_then(Json::as_array_mut);
                let block = content.and_then(|content| content.get_mut(*index)).and_then(Json::as_object_mut);
                if let Some(block) = block {
                    block.insert("arguments", snapshot.clone());
                }
                continue;
            }
            call.committed = true;
            apply.push((*index, changes));
        }
        assign_json(change, generation, Seg::key("message"), message, Some(&commit.plan))?;
        path.push(Seg::key("content"));
        for (index, changes) in apply {
            let mut block = path.clone();
            block.push(Seg::Index(index as u64));
            for json_change in changes {
                apply_change(change, &block, json_change)?;
            }
        }
        Ok(())
    }

    /// Put back what a failed commit took, so the next commit applies it first.
    pub fn restore(&mut self, commit: &PartialCommit) {
        for (index, (changes, _)) in &commit.tool_calls {
            if let Some(call) = self.tool_calls.get_mut(index) {
                let later = std::mem::take(&mut call.changes);
                call.changes = changes.iter().cloned().chain(later).collect();
            }
        }
    }
}

/// Apply one argument change to the committed arguments of the block at `block`.
fn apply_change(change: &mut Change, block: &[Seg], json_change: &JsonChange) -> ChordResult<()> {
    let mut parent = block.to_vec();
    parent.push(Seg::key("arguments"));
    let Some((last, inner)) = path_of(json_change).split_last() else {
        // Only an object root is tracked, and only a set starts it.
        if let JsonChange::Set { value, .. } = json_change
            && let Some(block) = node_mut(change.state()?, block).and_then(Json::as_object_mut)
        {
            block.insert("arguments", Json::from(value));
        }
        return Ok(());
    };
    parent.extend(inner.iter().map(segment));
    let Some(container) = node_mut(change.state()?, &parent) else { return Ok(()) };
    match (json_change, last) {
        (JsonChange::Set { value, .. }, PathSegment::Key(key)) => {
            if let Some(members) = container.as_object_mut() {
                members.insert(key.as_str(), Json::from(value));
            }
        }
        (JsonChange::Set { value, .. }, PathSegment::Index(index)) => {
            if let Some(items) = container.as_array_mut() {
                if *index == items.len() {
                    items.push(Json::from(value));
                } else if let Some(item) = items.get_mut(*index) {
                    *item = Json::from(value);
                }
            }
        }
        // `append_text()` takes object members; a string in an array is assigned, which compares it whole.
        (JsonChange::Append { text, .. }, PathSegment::Index(index)) => {
            let item = container.as_array_mut().and_then(|items| items.get_mut(*index));
            if let Some(item) = item
                && let Some(current) = item.as_str()
            {
                *item = Json::from(format!("{current}{text}"));
            }
        }
        (JsonChange::Append { text, .. }, PathSegment::Key(key)) => change.append_text(&parent, key, text)?,
    }
    Ok(())
}

fn segment(segment: &PathSegment) -> Seg {
    match segment {
        PathSegment::Key(key) => Seg::key(key.as_str()),
        PathSegment::Index(index) => Seg::Index(*index as u64),
    }
}

#[cfg(test)]
#[path = "partial_changes_tests.rs"]
mod tests;
