//! A state cell holding any JSON value, validated against its schema at creation and (dev mode) on writes.

use std::sync::{Arc, Mutex};

use super::{SourceCore, SourceOptions, WriteGuard, compute_dependents, not_ready};
use crate::delta::{Op, Prepared, Tracker, diff_revisions, track};
use crate::error::{ChordError, ChordResult};
use crate::host2::object::Ctx;
use crate::host2::publish::hold_publications;
use crate::host2::schema::assert_valid;
use crate::host2::types::{EventListener, Readable, StateEvent, ValueListener};
use crate::json::Json;
use crate::sync::{Unsubscribe, lock};

/// Checks that a context may write an object's cells now.
pub(crate) type WriteCheck = Arc<dyn Fn(&Ctx) -> ChordResult<()> + Send + Sync>;

struct CellState {
    /// Replaced at a reload commit that changes the schema (state-version bump).
    schema: Json,
    tracker: Tracker,
    /// False until `initialize` for a cell declared with `init`: reads and writes fail `not_ready`.
    initialized: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ops,
    Reset,
}

/// A state cell. Writes take a context: [`Cell::change`] mutates a draft, [`Cell::update`] maps the value,
/// [`Cell::replace`] swaps it.
pub struct Cell {
    core: SourceCore,
    check: WriteCheck,
    state: Mutex<CellState>,
}

impl Cell {
    /// The first value, when given, is always validated against `schema`; without it the cell has no value
    /// until `initialize`.
    pub(crate) fn new(
        key: &str,
        schema: Json,
        first: Option<Json>,
        check: WriteCheck,
        options: SourceOptions,
    ) -> ChordResult<Arc<Self>> {
        if let Some(value) = &first {
            assert_valid("state", &format!("initial value of state {key}"), &schema, Some(value))?;
        }
        let initialized = first.is_some();
        let state = CellState { schema, tracker: track(first.unwrap_or_default()), initialized };
        Ok(Arc::new(Self { core: SourceCore::new(key.to_string(), options), check, state: Mutex::new(state) }))
    }

    /// Sets the first value of a cell declared with `init` (validated). Publishes nothing.
    pub(crate) fn initialize(&self, value: Json) -> ChordResult<()> {
        let mut state = lock(&self.state);
        if state.initialized {
            return Err(ChordError::error(format!("state {} is already initialized", self.core.key())));
        }
        assert_valid("state", &format!("initial value of state {}", self.core.key()), &state.schema, Some(&value))?;
        state.tracker = track(value);
        state.initialized = true;
        Ok(())
    }

    pub fn key(&self) -> &str {
        self.core.key()
    }

    pub fn schema(&self) -> Json {
        lock(&self.state).schema.clone()
    }

    pub(crate) fn core(&self) -> &SourceCore {
        &self.core
    }

    fn unready(&self) -> ChordError {
        not_ready(&format!("state {} (its init has not run)", self.core.key()))
    }

    /// Mutates a draft; only the mutated paths are published.
    pub fn change(&self, ctx: &Ctx, mutate: impl FnOnce(&mut Json) -> ChordResult<()>) -> ChordResult<()> {
        (self.check)(ctx)?;
        let write = self.core.options().lock.enter()?;
        let mut change = {
            let mut state = lock(&self.state);
            if !state.initialized {
                return Err(self.unready());
            }
            state.tracker.begin_change()
        };
        let prepared = match change.state().and_then(mutate) {
            Ok(()) => change.prepare(),
            Err(error) => Err(error),
        };
        match prepared {
            Ok(prepared) => self.publish(write, prepared, Kind::Ops, true, None),
            Err(error) => {
                change.abort();
                Err(error)
            }
        }
    }

    /// Replaces the value with what `next` returns (`n => n + 1`), always validated against the schema.
    pub fn update(&self, ctx: &Ctx, next: impl FnOnce(&Json) -> ChordResult<Json>) -> ChordResult<()> {
        (self.check)(ctx)?;
        let write = self.core.options().lock.enter()?;
        let current = self.value()?;
        let value = next(&current)?;
        if !value.same_ref(&current) {
            assert_valid("state", &format!("state {}", self.core.key()), &self.schema(), Some(&value))?;
        }
        let prepared = lock(&self.state).tracker.prepare_replace(value);
        self.publish(write, prepared, Kind::Ops, true, None)
    }

    /// Replaces the whole value. Subscribers get a structural diff when the new value shares containers with
    /// the old one (a new revision of the same state), otherwise one `r` op.
    pub fn replace(&self, ctx: &Ctx, value: Json) -> ChordResult<()> {
        self.replace_with(ctx, value, None)
    }

    /// [`Cell::replace`]; `diff` forces the diff (`Some(true)`) or the `r` op (`Some(false)`).
    pub fn replace_with(&self, ctx: &Ctx, value: Json, diff: Option<bool>) -> ChordResult<()> {
        (self.check)(ctx)?;
        let write = self.core.options().lock.enter()?;
        let before = self.value()?;
        let ops = diff.unwrap_or_else(|| shares_structure(&before, &value)).then(|| diff_revisions(&before, &value));
        let prepared = lock(&self.state).tracker.prepare_replace(value);
        self.publish(write, prepared, Kind::Ops, true, ops)
    }

    /// The schema a reload commit declared, for a value it kept.
    pub(crate) fn set_schema(&self, schema: Json) {
        lock(&self.state).schema = schema;
    }

    /// Installs a migrated value and its schema (reload commit). Subscribers receive a reset.
    pub(crate) fn reset(&self, value: Json, schema: Json) -> ChordResult<()> {
        let write = self.core.options().lock.enter()?;
        let prepared = {
            let mut state = lock(&self.state);
            state.schema = schema;
            state.tracker.prepare_replace(value)
        };
        self.publish(write, prepared, Kind::Reset, false, None)
    }

    /// Adopts this revision and every derived one, then queues all notifications (cell first).
    fn publish(
        &self,
        write: WriteGuard<'_>,
        prepared: Prepared,
        kind: Kind,
        validate: bool,
        ops: Option<Vec<Op>>,
    ) -> ChordResult<()> {
        if prepared.ops.is_empty() && kind == Kind::Ops {
            return lock(&self.state).tracker.adopt(&prepared);
        }
        let value = prepared.value.clone();
        let candidates = (|| {
            if validate {
                self.core.validate(Some(&self.schema()), &value)?;
            }
            compute_dependents(&self.core, &value, validate)
        })();
        let candidates = match candidates {
            Ok(candidates) => candidates,
            Err(error) => {
                prepared.abort();
                return Err(error);
            }
        };
        hold_publications(move || {
            lock(&self.state).tracker.adopt(&prepared)?;
            let event = match kind {
                Kind::Reset => StateEvent::Reset { value },
                Kind::Ops => {
                    let ops = ops.filter(|ops| !ops.is_empty()).unwrap_or_else(|| prepared.ops.clone());
                    StateEvent::Ops { ops, value }
                }
            };
            self.core.emit(event);
            for (derived, next) in candidates {
                derived.install(next);
            }
            drop(write);
            Ok(())
        })
    }
}

impl Readable for Cell {
    fn value(&self) -> ChordResult<Json> {
        let state = lock(&self.state);
        if !state.initialized {
            return Err(self.unready());
        }
        Ok(state.tracker.value().clone())
    }

    fn subscribe(&self, listener: ValueListener) -> Unsubscribe {
        self.core.subscribe(listener, self.value().ok())
    }

    fn events(&self, listener: EventListener) -> Unsubscribe {
        self.core.events(listener, self.value().ok())
    }

    fn source(&self) -> Option<&SourceCore> {
        Some(&self.core)
    }
}

/// True if `after` reuses a container of `before` at the same key: a new revision of the same state.
fn shares_structure(before: &Json, after: &Json) -> bool {
    if before.same_ref(after) {
        return false;
    }
    let reused = |old: Option<&Json>, child: &Json| child.is_container() && old.is_some_and(|old| old.same_ref(child));
    match (before, after) {
        (Json::Object(old), Json::Object(new)) => new.iter().any(|(key, child)| reused(old.get(key), child)),
        (Json::Array(old), Json::Array(new)) => {
            new.iter().enumerate().any(|(index, child)| reused(old.get(index), child))
        }
        _ => false,
    }
}
