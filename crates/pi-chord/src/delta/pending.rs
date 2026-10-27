//! The pending queue: the ops a slow consumer of one state has not taken yet, folded while they wait.

use std::collections::HashMap;

use super::{Op, Path};
use crate::json::Json;

/// What [`PendingOps::take`] hands over: the pending ops, folded, or `Replace` after an overflow, which the owner of the
/// queue turns into a root replacement with the current value.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingTake {
    Ops(Vec<Op>),
    Replace,
}

/// Pending ops past which only "replace with the current value" stays pending.
pub const MAX_PENDING_OPS: usize = 10_000;

/// One folded op. Appends that fold into an earlier op grow a buffer, so a run of them costs O(text), not O(length).
enum Entry {
    Op(Op),
    Append(Path, String),
    SetText(Path, String),
}

impl Entry {
    fn into_op(self) -> Op {
        match self {
            Self::Op(op) => op,
            Self::Append(path, text) => Op::Append(path, text.into()),
            Self::SetText(path, text) => Op::Set(path, Json::from(text)),
        }
    }
}

#[derive(Default)]
enum State {
    #[default]
    Empty,
    /// One batch, kept as received: a consumer that keeps up never pays for folding.
    Batch(Vec<Op>),
    /// `leaves` maps a path to the position of the last pending leaf op on that path.
    Folding { entries: Vec<Entry>, leaves: HashMap<Path, usize> },
    /// Overflowed: nothing more accumulates until `take()` or a batch that starts with `r`.
    Replace,
}

/// Pending ops of one state for one consumer, raw and folded: a leaf op (an append, or a set of a primitive) merges
/// into the last pending leaf op on its path, any other op clears that index, and `r` drops everything before it. Past
/// [`MAX_PENDING_OPS`] only "replace with the current value" stays pending.
#[derive(Default)]
pub struct PendingOps {
    state: State,
}

impl PendingOps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue the ops of one commit.
    pub fn push(&mut self, ops: Vec<Op>) {
        if ops.is_empty() {
            return;
        }
        // A batch that starts with `r` drops everything pending, also an overflow, and is kept as received.
        if ops[0].is_replace() {
            self.state = State::Batch(ops);
            return;
        }
        self.state = match std::mem::take(&mut self.state) {
            State::Empty => State::Batch(ops),
            State::Batch(first) => {
                let (mut entries, mut leaves) = (Vec::new(), HashMap::new());
                match fold(first, &mut entries, &mut leaves).and_then(|()| fold(ops, &mut entries, &mut leaves)) {
                    Some(()) => State::Folding { entries, leaves },
                    None => State::Replace,
                }
            }
            State::Folding { mut entries, mut leaves } => match fold(ops, &mut entries, &mut leaves) {
                Some(()) => State::Folding { entries, leaves },
                None => State::Replace,
            },
            State::Replace => State::Replace,
        };
    }

    /// Pending ops after folding; 1 once overflowed (the replacement the owner sends). One batch counts as received.
    pub fn size(&self) -> usize {
        match &self.state {
            State::Empty => 0,
            State::Batch(ops) => ops.len(),
            State::Folding { entries, .. } => entries.len(),
            State::Replace => 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self.state, State::Empty)
    }

    /// Hand over everything pending and start empty.
    pub fn take(&mut self) -> PendingTake {
        match std::mem::take(&mut self.state) {
            State::Empty => PendingTake::Ops(Vec::new()),
            State::Batch(ops) => PendingTake::Ops(ops),
            State::Folding { entries, .. } => PendingTake::Ops(entries.into_iter().map(Entry::into_op).collect()),
            State::Replace => PendingTake::Replace,
        }
    }
}

/// Fold `ops` into `entries`; `None` once more than [`MAX_PENDING_OPS`] are pending.
fn fold(ops: Vec<Op>, entries: &mut Vec<Entry>, leaves: &mut HashMap<Path, usize>) -> Option<()> {
    for op in ops {
        let leaf = match &op {
            Op::Replace(_) => {
                entries.clear();
                leaves.clear();
                false
            }
            Op::Append(..) => true,
            Op::Set(_, value) => {
                let primitive = !matches!(value, Json::Array(_) | Json::Object(_));
                if !primitive {
                    leaves.clear();
                }
                primitive
            }
            _ => {
                leaves.clear();
                false
            }
        };
        if leaf && merge_leaf(&op, entries, leaves) {
            continue;
        }
        if leaf {
            leaves.insert(op.path().to_vec(), entries.len());
        }
        entries.push(Entry::Op(op));
        if entries.len() > MAX_PENDING_OPS {
            return None;
        }
    }
    Some(())
}

/// Merge the leaf op `op` into the last pending leaf op on its path; whether it merged.
fn merge_leaf(op: &Op, entries: &mut [Entry], leaves: &HashMap<Path, usize>) -> bool {
    // The index points at leaf ops only.
    let Some(entry) = leaves.get(op.path()).map(|&at| &mut entries[at]) else { return false };
    let text = match op {
        Op::Set(..) => {
            *entry = Entry::Op(op.clone());
            return true;
        }
        Op::Append(_, text) => text,
        _ => return false,
    };
    match entry {
        Entry::Append(_, buffer) | Entry::SetText(_, buffer) => buffer.push_str(text),
        Entry::Op(Op::Append(path, before)) => *entry = Entry::Append(path.clone(), format!("{before}{text}")),
        // Only a string set takes an append; the tracker never appends to another primitive.
        Entry::Op(Op::Set(path, Json::String(before))) => {
            *entry = Entry::SetText(path.clone(), format!("{before}{text}"));
        }
        Entry::Op(_) => return false,
    }
    true
}
