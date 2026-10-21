//! A synchronous derivation. Sources are object cells and derivations (recomputed in the step of the write) or
//! other readables such as reference state members (recomputed when they publish). Subscribers receive diffs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use super::{SourceCore, SourceOptions, not_ready, unhydrated};
use crate::delta::{Op, diff_revisions};
use crate::error::ChordResult;
use crate::host2::publish::hold_publications;
use crate::host2::types::{EventListener, Readable, ReadableRef, StateEvent, ValueListener};
use crate::json::Json;
use crate::sync::{BoxFuture, Unsubscribe, lock};

/// The function of a derivation: the values of its sources, in order, to its value.
pub type DeriveFn = Arc<dyn Fn(&[Json]) -> ChordResult<Json> + Send + Sync>;

pub struct Derived {
    core: SourceCore,
    sources: Vec<ReadableRef>,
    compute: DeriveFn,
    /// None until `attach`.
    value: Mutex<Option<Json>>,
    external: Mutex<Vec<Unsubscribe>>,
    me: Weak<Derived>,
}

impl Derived {
    pub(crate) fn new(key: String, sources: Vec<ReadableRef>, compute: DeriveFn, options: SourceOptions) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            core: SourceCore::new(key, options),
            sources,
            compute,
            value: Mutex::default(),
            external: Mutex::default(),
            me: me.clone(),
        })
    }

    pub fn key(&self) -> &str {
        self.core.key()
    }

    pub(crate) fn core(&self) -> &SourceCore {
        &self.core
    }

    pub(crate) fn compute(&self, read: impl Fn(&ReadableRef) -> ChordResult<Json>) -> ChordResult<Json> {
        let values = self.sources.iter().map(read).collect::<ChordResult<Vec<_>>>()?;
        (self.compute)(&values)
    }

    /// Computes the first value (or takes it from `precomputed`, a reload's validated dry run, by source id)
    /// and starts following the sources. Never inside a factory.
    pub(crate) fn attach(&self, precomputed: Option<&HashMap<usize, Json>>) -> ChordResult<()> {
        let value = match precomputed.and_then(|values| values.get(&self.core.id())) {
            Some(value) => value.clone(),
            None => self.compute(|source| source.value())?,
        };
        *lock(&self.value) = Some(value);
        for source in &self.sources {
            match source.source() {
                Some(core) => core.add_dependent(&self.me),
                None => {
                    let me = self.me.clone();
                    let unsubscribe = source.subscribe(Arc::new(move |_| {
                        if let Some(derived) = me.upgrade() {
                            derived.refresh();
                        }
                    }));
                    lock(&self.external).push(unsubscribe);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn detach(&self) {
        for source in &self.sources {
            if let Some(core) = source.source() {
                core.remove_dependent(&self.me);
            }
        }
        let external = std::mem::take(&mut *lock(&self.external));
        for unsubscribe in external {
            unsubscribe.unsubscribe();
        }
    }

    /// Recomputes and republishes, dependents included. Errors are reported: the source write already happened.
    pub(crate) fn refresh(&self) {
        let result = (|| {
            let write = self.core.options().lock.enter()?;
            let next = self.compute(|source| source.value())?;
            let dependents = compute_dependents(&self.core, &next, true)?;
            hold_publications(move || {
                self.install(next);
                for (derived, value) in dependents {
                    derived.install(value);
                }
                drop(write);
            });
            Ok(())
        })();
        if let Err(error) = result {
            (self.core.options().report)(error);
        }
    }

    pub(crate) fn install(&self, next: Json) {
        let before = lock(&self.value).replace(next.clone());
        let ops = match before {
            Some(before) => diff_revisions(&before, &next),
            None => vec![Op::Replace(next.clone())],
        };
        if !ops.is_empty() {
            self.core.emit(StateEvent::Ops { ops, value: next });
        }
    }
}

impl Readable for Derived {
    fn value(&self) -> ChordResult<Json> {
        lock(&self.value).clone().ok_or_else(|| not_ready(&format!("derive {}", self.core.key())))
    }

    fn subscribe(&self, listener: ValueListener) -> Unsubscribe {
        self.core.subscribe(listener, lock(&self.value).clone())
    }

    fn events(&self, listener: EventListener) -> Unsubscribe {
        self.core.events(listener, lock(&self.value).clone())
    }

    /// Hydrated once every source is (a derivation over a remote member waits for its snapshot).
    fn hydrated(&self) -> bool {
        self.sources.iter().all(|source| source.hydrated())
    }

    fn when_hydrated(&self) -> BoxFuture<ChordResult<()>> {
        let pending: Vec<_> = unhydrated(&self.sources).iter().map(|source| source.when_hydrated()).collect();
        Box::pin(async move {
            futures::future::try_join_all(pending).await?;
            Ok(())
        })
    }

    fn source(&self) -> Option<&SourceCore> {
        Some(&self.core)
    }
}

/// Recomputes every transitive dependent of `changed` against candidate values, in creation order.
pub(crate) fn compute_dependents(
    changed: &SourceCore,
    value: &Json,
    validate: bool,
) -> ChordResult<Vec<(Arc<Derived>, Json)>> {
    fn visit(source: &SourceCore, seen: &mut HashSet<usize>, pending: &mut Vec<Arc<Derived>>) {
        for derived in source.dependents() {
            if seen.insert(derived.core.id()) {
                pending.push(derived.clone());
                visit(&derived.core, seen, pending);
            }
        }
    }
    let mut pending = Vec::new();
    visit(changed, &mut HashSet::new(), &mut pending);
    pending.sort_by_key(|derived| derived.core.seq);
    let mut values = HashMap::from([(changed.id(), value.clone())]);
    let mut result = Vec::with_capacity(pending.len());
    for derived in pending {
        let next = derived.compute(|source| match source.source().and_then(|core| values.get(&core.id())) {
            Some(value) => Ok(value.clone()),
            None => source.value(),
        })?;
        if validate {
            derived.core.validate(None, &next)?;
        }
        values.insert(derived.core.id(), next.clone());
        result.push((derived, next));
    }
    Ok(result)
}
