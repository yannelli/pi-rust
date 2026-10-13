//! Typed entry kinds (upstream `defineEntry`) and the built-in `pi.*` entries the Harness writes.

use std::fmt;
use std::marker::PhantomData;
use std::sync::LazyLock;

use pi_ai::Message;
use pi_chord::Json;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::types::{ContextEdit, DraftHead, EntryDraft, EntryRecord};

/// Typed entry kind. `D` types `data`; `()` marks a kind without data, whose records omit `data`.
pub struct Entry<D = ()> {
    kind: String,
    _marker: PhantomData<fn() -> D>,
}

impl<D> Clone for Entry<D> {
    fn clone(&self) -> Self {
        Self { kind: self.kind.clone(), _marker: PhantomData }
    }
}

impl<D> fmt::Debug for Entry<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Entry").field(&self.kind).finish()
    }
}

/// An entry record whose `data` is decoded as `D`.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedEntry<D> {
    pub record: EntryRecord,
    pub data: D,
}

/// Entry content of a typed kind; the token supplies `kind`.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedEntryDraft<D> {
    pub data: D,
    pub model: Option<Vec<Message>>,
    pub head: Option<DraftHead>,
    pub edits: Option<Vec<ContextEdit>>,
}

impl<D> TypedEntryDraft<D> {
    pub fn new(data: D) -> Self {
        Self { data, model: None, head: None, edits: None }
    }
}

/// Define a typed entry kind whose `is()` guard matches `EntryRecord.kind`.
pub fn define_entry<D: Serialize + DeserializeOwned>(kind: impl Into<String>) -> DurableResult<Entry<D>> {
    let kind = kind.into();
    if kind.is_empty() {
        return Err(DurableError::type_error("Entry kind must be a non-empty string"));
    }
    Ok(Entry { kind, _marker: PhantomData })
}

impl<D: Serialize + DeserializeOwned> Entry<D> {
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Whether `entry` is present and of this kind.
    pub fn is(&self, entry: Option<&EntryRecord>) -> bool {
        entry.is_some_and(|entry| entry.kind == self.kind)
    }

    /// Decode a record of this kind; `None` for another kind.
    pub fn decode(&self, entry: EntryRecord) -> DurableResult<Option<TypedEntry<D>>> {
        if entry.kind != self.kind {
            return Ok(None);
        }
        let data = entry.data.clone().unwrap_or(Json::Null).deserialize_into::<D>()?;
        Ok(Some(TypedEntry { record: entry, data }))
    }

    /// The raw draft for a typed draft. Data that encodes to `null` is omitted, as upstream omits `undefined`.
    pub fn draft(&self, value: TypedEntryDraft<D>) -> DurableResult<EntryDraft> {
        let data = encode(&value.data)?;
        Ok(EntryDraft {
            kind: self.kind.clone(),
            model: value.model,
            data: (!data.is_null()).then_some(data),
            head: value.head,
            edits: value.edits,
        })
    }
}

fn builtin<D: Serialize + DeserializeOwned>(kind: &str) -> Entry<D> {
    Entry { kind: kind.to_owned(), _marker: PhantomData }
}

/// Data of a `pi.tool-result` entry: the structured diagnostics, possibly none.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolResultData {
    pub diagnostics: Vec<crate::harness::ToolDiagnostic>,
}

/// Data of a `pi.compaction` entry.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompactionData {
    pub reason: crate::harness::CompactionReason,
}

/// User input: `model` is `[UserMessage]`. Written by submissions.
pub static USER_ENTRY: LazyLock<Entry> = LazyLock::new(|| builtin("pi.user"));
/// Provider result with any stop reason: `model` is `[AssistantMessage]`. Written by generation.
pub static ASSISTANT_ENTRY: LazyLock<Entry> = LazyLock::new(|| builtin("pi.assistant"));
/// Positional prompt and tool change: `model` is `[SystemMessage]` with empty `content`.
pub static SYSTEM_ENTRY: LazyLock<Entry> = LazyLock::new(|| builtin("pi.system"));
/// Tool result: `model` is `[ToolResultMessage]`, whose content ends with the rendered diagnostics block.
pub static TOOL_RESULT_ENTRY: LazyLock<Entry<ToolResultData>> = LazyLock::new(|| builtin("pi.tool-result"));
/// Start of a new context: always `head: "self"`, with an optional user message carrying the handoff text.
pub static RESET_ENTRY: LazyLock<Entry> = LazyLock::new(|| builtin("pi.reset"));
/// Compaction summary: `model` is `[UserMessage]` with the wrapped summary, `head` the first kept entry.
pub static COMPACTION_ENTRY: LazyLock<Entry<CompactionData>> = LazyLock::new(|| builtin("pi.compaction"));
