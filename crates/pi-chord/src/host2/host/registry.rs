//! Registry tables: built from the registrations of live objects, one set per code version, and the typed
//! views code reads them through.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use indexmap::IndexMap;

use super::{ErasedListener, ErasedTable, HostImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::{ChordErrorCode, invalid};
use crate::host2::exec::{ErrorHandler, guarded};
use crate::host2::object::{Decls, HostCore, ObjectImpl, ObjectStatus};
use crate::host2::publish::enqueue;
use crate::host2::registry::{Conflict, Registration, Registry, RegistryTable, RegistryView, TableListener};
use crate::sync::Unsubscribe;

/// A host's view of one registry.
pub struct HostRegistry<V> {
    host: Weak<HostImpl>,
    name: String,
    value: PhantomData<fn() -> V>,
}

impl<V> HostRegistry<V> {
    pub(super) fn new(host: Weak<HostImpl>, registry: &Registry<V>) -> Self {
        Self { host, name: registry.name().to_string(), value: PhantomData }
    }
}

fn typed<V: Clone + 'static>(table: Option<&ErasedTable>) -> RegistryTable<V> {
    let entries = table.into_iter().flat_map(|table| table.iter());
    let typed = entries.filter_map(|(key, value)| value.downcast_ref::<V>().map(|value| (key.clone(), value.clone())));
    Arc::new(typed.collect())
}

/// The same keys in the same order with the same values (by identity).
fn same_entries(a: Option<&ErasedTable>, b: Option<&ErasedTable>) -> bool {
    let empty = IndexMap::new();
    let (a, b) = (a.map_or(&empty, |table| &**table), b.map_or(&empty, |table| &**table));
    a.len() == b.len() && a.iter().zip(b).all(|((key_a, a), (key_b, b))| key_a == key_b && Arc::ptr_eq(a, b))
}

fn registrations(decls: &Decls) -> Vec<Registration> {
    decls.data().registrations.values().cloned().collect()
}

impl<V: Clone + Send + Sync + 'static> RegistryView<V> for HostRegistry<V> {
    fn table(&self, code_version: Option<i64>) -> ChordResult<RegistryTable<V>> {
        let Some(host) = self.host.upgrade() else { return Ok(typed(None)) };
        let state = host.state();
        let id = code_version.unwrap_or(state.current);
        let Some(version) = state.code_versions.get(&id) else {
            let message = format!("code version {id} is retired");
            return Err(ChordError::code_only(ChordErrorCode::Superseded, message));
        };
        Ok(typed(version.tables.get(&self.name)))
    }

    fn subscribe(&self, listener: TableListener<V>) -> Unsubscribe {
        let Some(host) = self.host.upgrade() else { return Unsubscribe::noop() };
        let listener: Arc<TableListener<V>> = Arc::new(listener);
        let wrapped = listener.clone();
        let erased: ErasedListener =
            Arc::new(move |table: &ErasedTable, code_version| wrapped(&typed(Some(table)), code_version));
        let (id, table, current) = {
            let mut state = host.state();
            state.next_subscriber += 1;
            let id = state.next_subscriber;
            state.subscribers.entry(self.name.clone()).or_default().insert(id, erased);
            let current = state.current;
            let table = state.code_versions.get(&current).and_then(|version| version.tables.get(&self.name).cloned());
            (id, table, current)
        };
        listener(&typed(table.as_ref()), current);
        let (host, name) = (self.host.clone(), self.name.clone());
        Unsubscribe::new(move || {
            if let Some(host) = host.upgrade()
                && let Some(listeners) = host.state().subscribers.get_mut(&name)
            {
                listeners.shift_remove(&id);
            }
        })
    }
}

impl HostImpl {
    pub(crate) fn registry<V>(&self, registry: &Registry<V>) -> HostRegistry<V> {
        HostRegistry::new(self.me.clone(), registry)
    }

    /// Live objects with their current registrations; objects waiting for a reload's commit only with `candidates`.
    fn live_registrations(&self, candidates: bool) -> Vec<(Arc<ObjectImpl>, Vec<Registration>)> {
        let objects: Vec<Arc<ObjectImpl>> = self.state().objects.values().cloned().collect();
        let live = objects
            .into_iter()
            .filter(|object| object.status() == ObjectStatus::Live && (candidates || !object.deferred_runtime()));
        live.map(|object| {
            let registrations = registrations(&object.decls());
            (object, registrations)
        })
        .collect()
    }

    /// Builds registry tables from (object, registrations) pairs. Fails `invalid/registry_conflict`.
    pub(crate) fn build_tables(
        &self,
        entries: Vec<(Arc<ObjectImpl>, Vec<Registration>)>,
    ) -> ChordResult<HashMap<String, ErasedTable>> {
        let mut by_registry: IndexMap<String, IndexMap<String, (Registration, Arc<ObjectImpl>)>> = IndexMap::new();
        let mut state = self.state();
        for (object, registrations) in entries {
            for registration in registrations {
                let name = registration.registry.name().to_string();
                let entries = by_registry.entry(name.clone()).or_default();
                if let Some((_, existing)) = entries.get(&registration.key)
                    && !Arc::ptr_eq(existing, &object)
                    && registration.registry.conflict() == Conflict::Error
                {
                    let (first, second) = (existing.address(), object.address());
                    let message = format!("{name}: {} registered by {first} and {second}", registration.key);
                    return Err(invalid("registry_conflict", message, None));
                }
                let order = state.order.entry(name).or_default();
                let next = order.len();
                order.entry(registration.key.clone()).or_insert(next);
                entries.insert(registration.key.clone(), (registration, object.clone()));
            }
        }
        let mut tables = HashMap::new();
        for (name, entries) in by_registry {
            let order = state.order.get(&name).cloned().unwrap_or_default();
            let mut sorted: Vec<_> = entries.into_iter().collect();
            sorted.sort_by_key(|(key, _)| order.get(key).copied().unwrap_or(usize::MAX));
            let table = sorted.into_iter().map(|(key, (registration, _))| (key, registration.value)).collect();
            tables.insert(name, Arc::new(table));
        }
        Ok(tables)
    }

    pub(super) fn check_registered(&self, object: &Arc<ObjectImpl>, decls: &Decls) -> ChordResult<()> {
        if decls.data().registrations.is_empty() {
            return Ok(());
        }
        let mut entries = self.live_registrations(false);
        entries.retain(|(other, _)| !Arc::ptr_eq(other, object));
        entries.push((object.clone(), registrations(decls)));
        self.build_tables(entries).map(|_| ())
    }

    pub(super) fn rebuild_tables(&self, including: Option<&Arc<ObjectImpl>>) {
        let mut entries = self.live_registrations(false);
        if let Some(including) = including {
            entries.retain(|(object, _)| !Arc::ptr_eq(object, including));
            entries.push((including.clone(), registrations(&including.decls())));
        }
        match self.build_tables(entries) {
            Ok(tables) => {
                let current = self.state().current;
                self.install_tables(current, tables, false);
            }
            Err(error) => self.report(error),
        }
    }

    /// Installs tables for a code version; subscribers hear about changed tables of the current one.
    pub(crate) fn install_tables(&self, code_version: i64, tables: HashMap<String, ErasedTable>, all: bool) {
        let tables = Arc::new(tables);
        let mut notify = Vec::new();
        {
            let mut state = self.state();
            let current = state.current;
            let Some(version) = state.code_versions.get_mut(&code_version) else { return };
            let before = std::mem::replace(&mut version.tables, tables.clone());
            if code_version != current {
                return;
            }
            for (name, listeners) in &state.subscribers {
                let next = tables.get(name);
                if !all && same_entries(before.get(name), next) {
                    continue;
                }
                let listeners: Vec<_> = listeners.values().cloned().collect();
                notify.push((listeners, next.cloned().unwrap_or_default()));
            }
        }
        let report: ErrorHandler = {
            let host = self.core();
            Arc::new(move |error| host.report(error))
        };
        for (listeners, table) in notify {
            let report = report.clone();
            enqueue(move || {
                for listener in listeners {
                    guarded(&report, || listener(&table, code_version));
                }
            });
        }
    }
}
