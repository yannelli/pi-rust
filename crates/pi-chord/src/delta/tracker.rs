//! Transactional revision tracking. A draft is a structurally shared copy of the base; `prepare`
//! diffs it against the base and materializes the candidate from that batch, so replicas
//! reproduce it exactly. Adoption is a pointer swap.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use super::diff::{AppendHint, diff_draft_ordered};
use super::{Op, Path, Seg, apply_immutable};
use crate::error::{ChordError, ChordResult};
use crate::json::Json;

const OPEN: u8 = 0;
const PREPARED: u8 = 1;
const CONSUMED: u8 = 2;
const ABORTED: u8 = 3;
const STALE: u8 = 4;

type Status = Arc<AtomicU8>;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn settled_error() -> ChordError {
    ChordError::type_error("Cannot use a settled overlay")
}

/// A prepared candidate revision. Preparation does not change authority; adoption does.
#[derive(Clone)]
pub struct Prepared {
    owner: u64,
    status: Status,
    base_revision: u64,
    /// The revision this batch applies to.
    pub base: Json,
    /// The candidate; equal to `base` (same identity) when `ops` is empty.
    pub value: Json,
    /// The exact batch that turns `base` into `value`.
    pub ops: Vec<Op>,
}

impl Prepared {
    pub fn base_revision(&self) -> u64 {
        self.base_revision
    }

    /// Prevent adoption. The candidate stays readable.
    pub fn abort(&self) {
        let _ = self.status.compare_exchange(PREPARED, ABORTED, Ordering::SeqCst, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared").field("value", &self.value).field("ops", &self.ops).finish()
    }
}

/// One open transaction over a tracker revision.
pub struct Change {
    owner: u64,
    status: Status,
    base_revision: u64,
    /// Both are dropped once the change settles or goes stale, so held changes keep nothing alive.
    base: Option<Json>,
    draft: Option<Json>,
    settled: bool,
    prepared_status: Option<Status>,
    /// String members `append_text()` appended to, by path.
    appends: HashMap<Path, AppendHint>,
}

impl Change {
    /// The mutable draft. Fails once the change is prepared, aborted, or made stale by an adoption.
    pub fn state(&mut self) -> ChordResult<&mut Json> {
        match self.status.load(Ordering::SeqCst) {
            OPEN => self.draft.as_mut().ok_or_else(settled_error),
            _ => {
                self.release();
                Err(settled_error())
            }
        }
    }

    fn release(&mut self) {
        self.draft = None;
        self.base = None;
        self.appends.clear();
    }

    /// Append `text` to the string at `key` of the object at `path` in the draft. The change records it as one append
    /// op (appends to one key join) without comparing the old and new strings; a plain assignment or deletion of the
    /// key in the same change falls back to that comparison. The new string is still a copy: JSON strings are shared.
    pub fn append_text(&mut self, path: &[Seg], key: &str, text: &str) -> ChordResult<()> {
        let needs = || ChordError::type_error(format!("appendText() needs a string at {}", quoted(key)));
        let draft = self.state()?;
        let mut node = draft;
        for segment in path {
            node = match segment {
                Seg::Key(name) => node.get_mut(name),
                Seg::Index(index) => node.as_array_mut().and_then(|items| items.get_mut(*index as usize)),
            }
            .ok_or_else(|| ChordError::type_error("appendText() needs a draft"))?;
        }
        let object = match node {
            Json::Object(_) => node.as_object_mut().expect("an object"),
            Json::Array(_) => return Err(needs()),
            _ => return Err(ChordError::type_error("appendText() needs a draft")),
        };
        let Some(Json::String(current)) = object.get(key) else { return Err(needs()) };
        if text.is_empty() {
            return Ok(());
        }
        let current = current.clone();
        let after: Arc<str> = format!("{current}{text}").into();
        object.insert(key, Json::String(after.clone()));
        let mut full = path.to_vec();
        full.push(Seg::key(key));
        match self.appends.get_mut(&full).filter(|hint| Arc::ptr_eq(&hint.after, &current)) {
            Some(hint) => {
                hint.text = format!("{}{text}", hint.text).into();
                hint.after = after;
            }
            None => match self.base.as_ref().and_then(|base| string_at(base, &full)) {
                // Unchanged in this change so far: the append is all that differs.
                Some(base) if Arc::ptr_eq(&base, &current) => {
                    self.appends.insert(full, AppendHint { base, after, text: text.into() });
                }
                _ => {
                    self.appends.remove(&full);
                }
            },
        }
        Ok(())
    }

    /// Read the draft without changing it.
    pub fn peek(&self) -> ChordResult<&Json> {
        match self.status.load(Ordering::SeqCst) {
            OPEN | PREPARED => self.draft.as_ref().ok_or_else(settled_error),
            _ => Err(settled_error()),
        }
    }

    /// Materialize the candidate and its exact operation batch. Draft access ends here.
    pub fn prepare(&mut self) -> ChordResult<Prepared> {
        if self.settled {
            return Err(ChordError::error("Change has already been settled"));
        }
        match self.status.load(Ordering::SeqCst) {
            OPEN => {}
            PREPARED => return Err(ChordError::type_error("Prepared overlays are read-only")),
            _ => {
                self.release();
                return Err(settled_error());
            }
        }
        self.settled = true;
        let (Some(draft), Some(base)) = (self.draft.take(), self.base.take()) else { return Err(settled_error()) };
        let ops = diff_draft_ordered(&base, &draft, std::mem::take(&mut self.appends));
        drop(draft);
        let value = if ops.is_empty() {
            base.clone()
        } else {
            match apply_immutable(Some(&base), &ops) {
                Ok(value) => value,
                Err(error) => {
                    self.status.store(ABORTED, Ordering::SeqCst);
                    return Err(error);
                }
            }
        };
        self.status.store(PREPARED, Ordering::SeqCst);
        self.prepared_status = Some(self.status.clone());
        Ok(Prepared {
            owner: self.owner,
            status: self.status.clone(),
            base_revision: self.base_revision,
            base,
            value,
            ops,
        })
    }

    /// Discard the change, or prevent adoption of its preparation. Idempotent.
    pub fn abort(&mut self) {
        if self.settled {
            if let Some(status) = self.prepared_status.take() {
                let _ = status.compare_exchange(PREPARED, ABORTED, Ordering::SeqCst, Ordering::SeqCst);
            }
            return;
        }
        self.settled = true;
        self.release();
        let _ = self.status.compare_exchange(OPEN, ABORTED, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// One revision sequence over an owned immutable JSON root.
pub struct Tracker {
    owner: u64,
    value: Json,
    revision: u64,
    contexts: Vec<Weak<AtomicU8>>,
}

/// Take immutable ownership of a root in O(1).
pub fn track(initial: Json) -> Tracker {
    Tracker { owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed), value: initial, revision: 0, contexts: Vec::new() }
}

impl Tracker {
    /// The latest adopted revision.
    pub fn value(&self) -> &Json {
        &self.value
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn register(&mut self, status: &Status) {
        if self.contexts.len() >= 256 {
            self.contexts.retain(|weak| weak.strong_count() > 0);
        }
        self.contexts.push(Arc::downgrade(status));
    }

    /// Open a draft over the current revision.
    pub fn begin_change(&mut self) -> Change {
        let status = Arc::new(AtomicU8::new(OPEN));
        self.register(&status);
        Change {
            owner: self.owner,
            status,
            base_revision: self.revision,
            base: Some(self.value.clone()),
            draft: Some(self.value.clone()),
            settled: false,
            prepared_status: None,
            appends: HashMap::new(),
        }
    }

    /// Prepare a whole-root replacement; a deeply equal value keeps the current root.
    pub fn prepare_replace(&mut self, value: Json) -> Prepared {
        let status = Arc::new(AtomicU8::new(PREPARED));
        self.register(&status);
        let noop = value == self.value;
        Prepared {
            owner: self.owner,
            status,
            base_revision: self.revision,
            base: self.value.clone(),
            value: if noop { self.value.clone() } else { value.clone() },
            ops: if noop { Vec::new() } else { vec![Op::Replace(value)] },
        }
    }

    /// Check a preparation and swap the root pointer. Every competing change becomes stale.
    pub fn adopt(&mut self, prepared: &Prepared) -> ChordResult<()> {
        if prepared.owner != self.owner {
            return Err(ChordError::error("Prepared change belongs to a different tracker"));
        }
        match prepared.status.load(Ordering::SeqCst) {
            CONSUMED => return Err(ChordError::error("Prepared change has already been used")),
            ABORTED => return Err(ChordError::error("Prepared change has been aborted")),
            STALE => return Err(ChordError::error("Prepared change is stale")),
            PREPARED => {}
            _ => return Err(ChordError::error("Prepared change is not ready")),
        }
        if prepared.base_revision != self.revision || !self.value.same_ref(&prepared.base) {
            prepared.status.store(STALE, Ordering::SeqCst);
            return Err(ChordError::error("Prepared change is stale"));
        }
        self.value = prepared.value.clone();
        prepared.status.store(CONSUMED, Ordering::SeqCst);
        self.revision += 1;
        for weak in self.contexts.drain(..) {
            if let Some(status) = weak.upgrade()
                && !Arc::ptr_eq(&status, &prepared.status)
            {
                let _ = status.compare_exchange(OPEN, STALE, Ordering::SeqCst, Ordering::SeqCst);
                let _ = status.compare_exchange(PREPARED, STALE, Ordering::SeqCst, Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

fn quoted(key: &str) -> String {
    serde_json::to_string(key).unwrap_or_else(|_| key.to_owned())
}

/// The string at `path` in `value`, when there is one.
fn string_at(value: &Json, path: &[Seg]) -> Option<Arc<str>> {
    let mut node = value;
    for segment in path {
        node = match segment {
            Seg::Key(name) => node.get(name)?,
            Seg::Index(index) => node.as_array()?.get(*index as usize)?,
        };
    }
    match node {
        Json::String(text) => Some(text.clone()),
        _ => None,
    }
}
