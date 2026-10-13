//! Path interning between the tracker and a boundary. Use one encoder/decoder pair per state stream:
//! ids span batches, a base batch resets both dictionaries, and short forms never span batches.

use std::collections::{HashMap, HashSet};

use super::{Op, Path, PathRef, WireOp, assert_safe_path, path_error};
use crate::error::{ChordError, ChordResult};
use crate::json::Json;

/// Interns repeated paths on their second use and omits a path equal to the one just before.
#[derive(Default)]
pub struct Encoder {
    seen: HashSet<Path>,
    ids: HashMap<Path, u64>,
    next_id: u64,
}

/// Create a fresh encoder.
pub fn encoder() -> Encoder {
    Encoder::default()
}

fn to_wire(op: &Op, reference: Option<PathRef>) -> WireOp {
    match op {
        Op::Replace(value) => WireOp::Replace(value.clone()),
        Op::Set(_, value) => WireOp::Set(reference, value.clone()),
        Op::Delete(_) => WireOp::Delete(reference),
        Op::Append(_, text) => WireOp::Append(reference, text.clone()),
        Op::Truncate(_, count) => WireOp::Truncate(reference, *count),
        Op::Splice(_, index, remove, items) => WireOp::Splice(reference, *index, *remove, items.clone()),
        Op::Move(_, permutation) => WireOp::Move(reference, permutation.clone()),
    }
}

impl Encoder {
    pub fn encode(&mut self, ops: &[Op]) -> Vec<WireOp> {
        let mut out = Vec::with_capacity(ops.len());
        let mut previous: Option<&[super::Seg]> = None;
        for op in ops {
            if op.is_replace() {
                out.push(to_wire(op, None));
                self.seen.clear();
                self.ids.clear();
                self.next_id = 0;
                previous = None;
                continue;
            }
            let path = op.path();
            if previous == Some(path) {
                out.push(to_wire(op, None));
                continue;
            }
            let reference = if let Some(id) = self.ids.get(path) {
                PathRef::Id(*id)
            } else if self.seen.contains(path) {
                let id = self.next_id;
                self.next_id += 1;
                self.ids.insert(path.to_vec(), id);
                out.push(WireOp::Define(id, path.to_vec()));
                PathRef::Id(id)
            } else {
                self.seen.insert(path.to_vec());
                PathRef::Path(path.to_vec())
            };
            out.push(to_wire(op, Some(reference)));
            previous = Some(path);
        }
        out
    }
}

/// Restores [`Op`] tuples from one encoder's stream, validating as it goes.
#[derive(Default)]
pub struct Decoder {
    paths: HashMap<u64, Path>,
}

/// Create a fresh decoder.
pub fn decoder() -> Decoder {
    Decoder::default()
}

fn check_permutation(permutation: &[usize]) -> ChordResult<()> {
    let mut seen = vec![false; permutation.len()];
    for &index in permutation {
        if index >= permutation.len() || seen[index] {
            return Err(ChordError::type_error("m permutation is not a bijection"));
        }
        seen[index] = true;
    }
    Ok(())
}

impl Decoder {
    pub fn decode(&mut self, wire: &[WireOp]) -> ChordResult<Vec<Op>> {
        let mut out = Vec::with_capacity(wire.len());
        let mut previous: Option<Path> = None;
        for op in wire {
            let reference = match op {
                WireOp::Define(id, path) => {
                    assert_safe_path(path)?;
                    self.paths.insert(*id, path.clone());
                    continue;
                }
                WireOp::Replace(value) => {
                    out.push(Op::Replace(value.clone()));
                    self.paths.clear();
                    previous = None;
                    continue;
                }
                WireOp::Set(reference, _)
                | WireOp::Delete(reference)
                | WireOp::Append(reference, _)
                | WireOp::Truncate(reference, _)
                | WireOp::Splice(reference, ..)
                | WireOp::Move(reference, _) => reference,
            };
            let path = match reference {
                None => previous.clone().ok_or_else(|| path_error(&[]))?,
                Some(PathRef::Id(id)) => {
                    self.paths.get(id).cloned().ok_or_else(|| ChordError::Path { path: id.to_string() })?
                }
                Some(PathRef::Path(path)) => {
                    assert_safe_path(path)?;
                    path.clone()
                }
            };
            if reference.is_some() {
                previous = Some(path.clone());
            }
            let structural = matches!(op, WireOp::Splice(..) | WireOp::Move(..));
            if !structural && path.is_empty() {
                return Err(path_error(&path));
            }
            out.push(match op {
                WireOp::Set(_, value) => Op::Set(path, value.clone()),
                WireOp::Delete(_) => Op::Delete(path),
                WireOp::Append(_, text) => Op::Append(path, text.clone()),
                WireOp::Truncate(_, count) => Op::Truncate(path, *count),
                WireOp::Splice(_, index, remove, items) => Op::Splice(path, *index, *remove, items.clone()),
                WireOp::Move(_, permutation) => {
                    check_permutation(permutation)?;
                    Op::Move(path, permutation.clone())
                }
                WireOp::Replace(_) | WireOp::Define(..) => unreachable!(),
            });
        }
        Ok(out)
    }

    /// Parse and decode a JSON array of wire tuples.
    pub fn decode_json(&mut self, wire: &Json) -> ChordResult<Vec<Op>> {
        let ops = super::parse_wire_ops(wire)?;
        self.decode(&ops)
    }
}
