//! Revocable document drafts returned by `Tx::doc`.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_chord::delta::Change;

use super::line::lock;
use super::tx::TxInner;
use crate::documents::{DocValue, encode};
use crate::errors::DurableResult;

/// Untyped draft: the open Chord change of one document in one transaction. Cheap to clone.
#[derive(Clone)]
pub struct DraftCore {
    pub(crate) cell: Arc<Mutex<Change>>,
    pub(crate) tx: Arc<TxInner>,
}

impl DraftCore {
    /// Whether both handles are the same memoized draft.
    pub fn ptr_eq(&self, other: &DraftCore) -> bool {
        Arc::ptr_eq(&self.cell, &other.cell)
    }

    /// The current draft value. Fails once the transaction callback settled.
    pub fn value(&self) -> DurableResult<Json> {
        self.tx.assert_open()?;
        Ok(lock(&self.cell).peek()?.clone())
    }

    /// Mutate the draft in place. Chord diffs the draft against its base when the callback settles.
    /// `mutate` must not use this draft again (the draft is locked while it runs).
    pub fn update<R>(&self, mutate: impl FnOnce(&mut Json) -> R) -> DurableResult<R> {
        self.tx.assert_open()?;
        let mut change = lock(&self.cell);
        Ok(mutate(change.state()?))
    }

    /// Run `edit` on the open Chord change of this draft, for edits a JSON draft cannot make, such as
    /// `Change::append_text()`. `edit` must not use this draft again (the draft is locked while it runs).
    pub(crate) fn with_change<R>(&self, edit: impl FnOnce(&mut Change) -> R) -> DurableResult<R> {
        self.tx.assert_open()?;
        Ok(edit(&mut lock(&self.cell)))
    }

    /// Replace the draft value, keeping every unchanged subtree of the current draft.
    pub fn set(&self, value: Json) -> DurableResult<()> {
        self.update(|draft| assign_shared(draft, value))
    }
}

/// Typed draft. `get` deserializes the draft; `edit` deserializes, mutates, and writes back, keeping unchanged
/// subtrees shared so the Chord diff yields only the changed paths.
pub struct Draft<T> {
    core: DraftCore,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Draft<T> {
    fn clone(&self) -> Self {
        Self { core: self.core.clone(), _marker: PhantomData }
    }
}

impl<T: DocValue> Draft<T> {
    pub(crate) fn new(core: DraftCore) -> Self {
        Self { core, _marker: PhantomData }
    }

    pub fn core(&self) -> &DraftCore {
        &self.core
    }

    pub fn ptr_eq(&self, other: &Draft<T>) -> bool {
        self.core.ptr_eq(&other.core)
    }

    pub fn value(&self) -> DurableResult<Json> {
        self.core.value()
    }

    pub fn get(&self) -> DurableResult<T> {
        Ok(self.core.value()?.deserialize_into::<T>()?)
    }

    pub fn update<R>(&self, mutate: impl FnOnce(&mut Json) -> R) -> DurableResult<R> {
        self.core.update(mutate)
    }

    /// Deserialize, mutate, and write the result back. Fails before changing the draft when the result is not
    /// strict JSON.
    pub fn edit<R>(&self, mutate: impl FnOnce(&mut T) -> R) -> DurableResult<R> {
        let mut value = self.get()?;
        let result = mutate(&mut value);
        let encoded = encode(&value)?;
        self.core.set(encoded)?;
        Ok(result)
    }

    /// Replace the whole value.
    pub fn set(&self, value: &T) -> DurableResult<()> {
        self.core.set(encode(value)?)
    }
}

/// Write `value` into `target` in place, leaving equal subtrees untouched so they stay shared with the base.
pub(crate) fn assign_shared(target: &mut Json, value: Json) {
    if *target == value {
        return;
    }
    match (&mut *target, &value) {
        (Json::Object(_), Json::Object(next)) => {
            let next = next.clone();
            let Some(map) = target.as_object_mut() else { return };
            let stale: Vec<_> = map.keys().filter(|key| !next.contains_key(key)).cloned().collect();
            for key in stale {
                map.remove(&key);
            }
            for (key, item) in next.iter() {
                match map.get_mut(key) {
                    Some(existing) => assign_shared(existing, item.clone()),
                    None => {
                        map.insert(key.clone(), item.clone());
                    }
                }
            }
        }
        (Json::Array(current), Json::Array(next)) if current.len() == next.len() => {
            let next = next.clone();
            let Some(items) = target.as_array_mut() else { return };
            for (slot, item) in items.iter_mut().zip(next.iter()) {
                assign_shared(slot, item.clone());
            }
        }
        _ => *target = value,
    }
}
