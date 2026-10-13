//! Typed entry kinds (upstream `defineEntry`). The built-in `pi.*` entries come with the Harness.

use std::fmt;
use std::marker::PhantomData;

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
