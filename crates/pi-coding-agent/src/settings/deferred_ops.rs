//! Loading, trust, reload and the queued writes of an SDK `SettingsManager`, as upstream's
//! constructor, `setProjectTrusted`, `reload` and `enqueueWrite` do them.

use std::sync::{Arc, RwLock, RwLockWriteGuard};

use serde_json::{Map, Value};

use super::deferred::{
    Deferred, Read, Snapshot, Thrown, UNTRUSTED, host_text, layer, load, persist_text, recompute, record, sdk,
    set_failed, slot,
};
use super::manager::{SettingsError, SettingsManager, State};
use super::storage::{InMemorySettingsStorage, SettingsScope, SettingsStorage};

impl SettingsManager {
    /// An SDK manager: `reads` are the JavaScript reads of a host storage, `None` reads `storage`.
    pub(crate) fn sdk(storage: Arc<dyn SettingsStorage>, trusted: bool, reads: Option<(Read, Option<Read>)>) -> Self {
        let state = State { project_trusted: trusted, sdk: Some(Deferred::new(reads.is_some())), ..State::default() };
        let manager = Self { storage, state: RwLock::new(state) };
        let (global, project) = reads.unwrap_or_else(|| {
            (manager.read(SettingsScope::Global), trusted.then(|| manager.read(SettingsScope::Project)))
        });
        let mut state = manager.lock();
        for (scope, read) in [(SettingsScope::Global, Some(global)), (SettingsScope::Project, project)] {
            let Some(read) = read else { continue };
            match load(read) {
                Ok(map) => *layer(&mut state, scope) = map,
                Err(thrown) => {
                    record(&mut state, scope, manager.storage.path(scope), thrown);
                    set_failed(&mut state, scope, true);
                }
            }
        }
        recompute(&mut state);
        drop(state);
        manager
    }

    /// Upstream `inMemory`: the migrated copy of `settings` is the stored global text.
    pub(crate) fn sdk_in_memory(settings: Value, trusted: bool) -> Result<Self, String> {
        let text = match super::js::normalize(settings) {
            Value::Object(mut settings) => {
                super::manager::migrate(&mut settings);
                super::js::stringify(&settings)
            }
            array @ Value::Array(_) => super::js_text::stringify_value(&array),
            other => return Err(super::js_text::in_error(&other)),
        };
        let storage = InMemorySettingsStorage::default();
        storage.with_lock(SettingsScope::Global, &mut |_| Ok(Some(text.clone())))?;
        Ok(Self::sdk(Arc::new(storage), trusted, None))
    }

    fn read(&self, scope: SettingsScope) -> Read {
        let mut content = None;
        self.storage.with_lock(scope, &mut |current| {
            content = current.map(String::from);
            Ok(None)
        })?;
        Ok(content.map(Value::String))
    }

    pub(super) fn lock(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn host(&self) -> bool {
        self.state().sdk.as_ref().is_some_and(|sdk| sdk.host)
    }

    /// Upstream `reload` once the queue has drained; a Rust storage first writes what is still
    /// queued. A host storage needs `reads`; without them the layers stay as they are.
    pub(crate) fn sdk_reload(&self, reads: Option<(Read, Option<Read>)>) {
        if self.host() && reads.is_none() {
            return;
        }
        while reads.is_none() && !sdk(&mut self.lock()).pending.is_empty() {
            self.sdk_persist();
        }
        let trusted = self.is_project_trusted();
        let (global, project) = reads
            .unwrap_or_else(|| (self.read(SettingsScope::Global), trusted.then(|| self.read(SettingsScope::Project))));
        let mut state = self.lock();
        for (scope, read) in [(SettingsScope::Global, global), (SettingsScope::Project, project.unwrap_or(Ok(None)))] {
            match load(read) {
                Ok(map) => {
                    *layer(&mut state, scope) = map;
                    set_failed(&mut state, scope, false);
                    sdk(&mut state).epoch[slot(scope)] += 1;
                }
                Err(thrown) => {
                    set_failed(&mut state, scope, true);
                    record(&mut state, scope, self.storage.path(scope), thrown);
                }
            }
            if scope == SettingsScope::Global {
                sdk(&mut state).modified = Default::default();
            }
        }
        recompute(&mut state);
    }

    /// Upstream `setProjectTrusted`: distrust clears the project layer, trust loads it. A host
    /// storage passes its `read`; without one the layer stays as it is.
    pub(crate) fn sdk_set_trusted(&self, trusted: bool, read: Option<Read>) {
        if self.is_project_trusted() == trusted {
            return;
        }
        let read = match read {
            None if trusted && !self.host() => Some(self.read(SettingsScope::Project)),
            read => read,
        };
        let mut state = self.lock();
        state.project_trusted = trusted;
        let sdk = sdk(&mut state);
        sdk.modified[1] = Default::default();
        sdk.epoch[1] += 1;
        if !trusted {
            state.project = Map::new();
            state.project_load_failed = false;
        }
        if let Some(read) = read.filter(|_| trusted) {
            match load(read) {
                Ok(map) => {
                    state.project = map;
                    state.project_load_failed = false;
                }
                Err(thrown) => {
                    state.project = Map::new();
                    state.project_load_failed = true;
                    record(&mut state, SettingsScope::Project, self.storage.path(SettingsScope::Project), thrown);
                }
            }
        }
        recompute(&mut state);
    }

    /// Upstream `applyOverrides`: merged over the current settings until the next save or load.
    pub(crate) fn sdk_apply_overrides(&self, overrides: &Map<String, Value>) {
        super::deferred::apply_overrides(&mut self.lock(), overrides.clone());
    }

    /// Call `wake` when a snapshot is queued outside a JavaScript call, such as by a child session.
    pub(crate) fn sdk_watch(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        sdk(&mut self.lock()).notify = Some(wake);
    }

    /// Snapshots queued since the last claim; the JavaScript queue adds one write for each.
    pub(crate) fn sdk_claim(&self) -> usize {
        std::mem::take(&mut sdk(&mut self.lock()).unclaimed)
    }

    /// The next snapshot, and whether its scope may still be written (upstream checks project
    /// trust when the write runs).
    fn pop(&self) -> Option<(Snapshot, bool)> {
        let mut state = self.lock();
        let trusted = state.project_trusted;
        let snapshot = sdk(&mut state).pending.pop_front()?;
        let allowed = snapshot.scope == SettingsScope::Global || trusted;
        Some((snapshot, allowed))
    }

    /// One queued write through `storage`. Errors are recorded, as upstream's queue catches them.
    pub(crate) fn sdk_persist(&self) {
        let Some((snapshot, allowed)) = self.pop() else { return };
        let scope = snapshot.scope;
        let result = if allowed {
            self.storage.with_lock(scope, &mut |current| persist_text(current, &snapshot).map(Some))
        } else {
            Err(UNTRUSTED.to_string())
        };
        match result {
            Ok(()) => self.sdk_done(scope),
            Err(message) => self.sdk_record(scope, Thrown::from(message)),
        }
    }

    /// The next write of a host storage: its scope, and the trust error upstream's queue throws.
    pub(crate) fn sdk_take(&self) -> Option<(SettingsScope, Option<&'static str>)> {
        let (snapshot, allowed) = self.pop()?;
        let scope = snapshot.scope;
        sdk(&mut self.lock()).active = Some(snapshot);
        Some((scope, (!allowed).then_some(UNTRUSTED)))
    }

    /// The text of the taken write over `current`, inside the host storage's `withLock` call.
    pub(crate) fn sdk_text(&self, current: Option<Value>) -> Result<String, String> {
        let state = self.state();
        let snapshot = state.sdk.as_ref().and_then(|sdk| sdk.active.as_ref()).ok_or("no settings write is running")?;
        persist_text(host_text(current)?.as_deref(), snapshot)
    }

    /// Upstream `clearModifiedScope` after a write succeeded.
    pub(crate) fn sdk_done(&self, scope: SettingsScope) {
        sdk(&mut self.lock()).modified[slot(scope)] = Default::default();
    }

    /// Upstream `drainErrors`, with the JavaScript error token of each error.
    pub(crate) fn sdk_drain_errors(&self) -> Vec<(SettingsError, Option<u64>)> {
        let mut state = self.lock();
        let errors = std::mem::take(&mut state.errors);
        let mut tokens = state.sdk.as_mut().map(|sdk| std::mem::take(&mut sdk.tokens)).unwrap_or_default();
        tokens.resize(errors.len(), None);
        errors.into_iter().zip(tokens).collect()
    }

    /// An error the queue caught, kept for `drainErrors`.
    pub(crate) fn sdk_record(&self, scope: SettingsScope, thrown: Thrown) {
        record(&mut self.lock(), scope, self.storage.path(scope), thrown);
    }
}
