//! Port of upstream `registry.ts`: registries objects declare entries into, with one table per code version.

use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::ChordResult;
use crate::sync::Unsubscribe;

/// What happens when two objects register one key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Conflict {
    /// Rejects the second registration (`invalid/registry_conflict`).
    #[default]
    Error,
    /// Keeps the later registration.
    Last,
}

/// A registry of values of type `V`, identified by name.
#[derive(Debug)]
pub struct Registry<V> {
    name: Arc<str>,
    conflict: Conflict,
    value: PhantomData<fn() -> V>,
}

impl<V> Clone for Registry<V> {
    fn clone(&self) -> Self {
        Self { name: self.name.clone(), conflict: self.conflict, value: PhantomData }
    }
}

impl<V> Registry<V> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn conflict(&self) -> Conflict {
        self.conflict
    }
}

/// A registry; `conflict` defaults to [`Conflict::Error`].
pub fn define_registry<V>(name: &str, conflict: Option<Conflict>) -> Registry<V> {
    Registry { name: name.into(), conflict: conflict.unwrap_or_default(), value: PhantomData }
}

/// One code version's entries, in registration order.
pub type RegistryTable<V> = Arc<IndexMap<String, V>>;

pub type TableListener<V> = Box<dyn Fn(&RegistryTable<V>, i64) + Send + Sync>;

/// A host's view of one registry. Upstream passes a `ctx` and reads only its `codeVersion`; here the caller passes
/// that code version (`None`: the current one).
pub trait RegistryView<V>: Send + Sync {
    /// The table of `code_version`. Fails with `superseded` when that code version is retired.
    fn table(&self, code_version: Option<i64>) -> ChordResult<RegistryTable<V>>;

    fn get(&self, key: &str, code_version: Option<i64>) -> ChordResult<Option<V>>
    where
        V: Clone,
    {
        Ok(self.table(code_version)?.get(key).cloned())
    }

    /// Called with the current table immediately and whenever a new table is installed.
    fn subscribe(&self, listener: TableListener<V>) -> Unsubscribe;
}

/// A registry entry an object declares: the registry, the key and the (type-erased) value.
#[derive(Clone)]
pub struct Registration {
    pub registry: Registry<()>,
    pub key: String,
    pub value: Arc<dyn Any + Send + Sync>,
}

impl Registration {
    pub fn new<V: Any + Send + Sync>(registry: &Registry<V>, key: impl Into<String>, value: V) -> Self {
        let registry = Registry { name: registry.name.clone(), conflict: registry.conflict, value: PhantomData };
        Self { registry, key: key.into(), value: Arc::new(value) }
    }
}
