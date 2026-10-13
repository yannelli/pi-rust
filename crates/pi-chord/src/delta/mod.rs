//! Immutable revision tracking and operations over plain JSON (upstream `chord/delta`).
//!
//! Operation tuples are the form in memory, on the wire, and on disk; see `ops_json` for the encoding.

mod apply;
mod codec;
mod diff;
mod diff_array;
mod ops_json;
mod overlap;
mod tracker;

use std::fmt;
use std::sync::Arc;

pub(crate) use apply::path_error;
pub use apply::{apply, apply_immutable, apply_immutable_batches};
pub use codec::{Decoder, Encoder, decoder, encoder};
pub use diff::diff_revisions;
pub use ops_json::{assert_valid_op, assert_valid_wire_op, parse_op, parse_ops, parse_wire_op, parse_wire_ops};
pub use overlap::{overlap, overlap_with};
pub use tracker::{Change, Prepared, Tracker, track};

use crate::error::ChordError;
use crate::json::Json;

/// Segments that reach the JavaScript prototype chain. The tracker never emits them as paths.
pub const RESERVED_SEGMENTS: [&str; 3] = ["__proto__", "constructor", "prototype"];

pub(crate) fn is_reserved(key: &str) -> bool {
    RESERVED_SEGMENTS.contains(&key)
}

/// One path segment: an object key or a non-negative array index.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Seg {
    Key(Arc<str>),
    Index(u64),
}

impl Seg {
    pub fn key(key: impl Into<Arc<str>>) -> Self {
        Self::Key(key.into())
    }

    pub fn to_json(&self) -> Json {
        match self {
            Seg::Key(key) => Json::String(key.clone()),
            Seg::Index(index) => Json::from(*index),
        }
    }
}

impl From<&str> for Seg {
    fn from(key: &str) -> Self {
        Seg::Key(key.into())
    }
}

impl From<String> for Seg {
    fn from(key: String) -> Self {
        Seg::Key(key.into())
    }
}

impl From<usize> for Seg {
    fn from(index: usize) -> Self {
        Seg::Index(index as u64)
    }
}

impl fmt::Debug for Seg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Seg::Key(key) => write!(f, "{key:?}"),
            Seg::Index(index) => write!(f, "{index}"),
        }
    }
}

/// A path of object keys and array indices; empty addresses the root.
pub type Path = Vec<Seg>;

/// Build a [`Path`] from keys and indices: `path!["rows", 0usize, "value"]`.
#[macro_export]
macro_rules! path {
    () => { ::std::vec::Vec::<$crate::delta::Seg>::new() };
    ($($segment:expr),+ $(,)?) => { ::std::vec::Vec::<$crate::delta::Seg>::from([$($crate::delta::Seg::from($segment)),+]) };
}

pub(crate) fn path_to_json(path: &[Seg]) -> Json {
    Json::array(path.iter().map(Seg::to_json).collect())
}

/// One decoded operation. `Replace` is the only operation that replaces a whole value.
#[derive(Clone, PartialEq)]
pub enum Op {
    /// `["r", value]`
    Replace(Json),
    /// `["s", path, value]`: set an object property or array element.
    Set(Path, Json),
    /// `["d", path]`: delete an object property or remove an array element.
    Delete(Path),
    /// `["a", path, text]`: append to a string.
    Append(Path, Arc<str>),
    /// `["t", path, count]`: remove `count` UTF-16 code units from a string's front.
    Truncate(Path, u64),
    /// `["p", path, index, remove, items]`: splice an array.
    Splice(Path, u64, u64, Vec<Json>),
    /// `["m", path, permutation]`: reorder an array so `new[i] = old[permutation[i]]`.
    Move(Path, Vec<usize>),
}

impl Op {
    /// The single-letter verb.
    pub fn verb(&self) -> &'static str {
        match self {
            Op::Replace(_) => "r",
            Op::Set(..) => "s",
            Op::Delete(_) => "d",
            Op::Append(..) => "a",
            Op::Truncate(..) => "t",
            Op::Splice(..) => "p",
            Op::Move(..) => "m",
        }
    }

    /// The target path; empty for `Replace`.
    pub fn path(&self) -> &[Seg] {
        match self {
            Op::Replace(_) => &[],
            Op::Set(path, _)
            | Op::Delete(path)
            | Op::Append(path, _)
            | Op::Truncate(path, _)
            | Op::Splice(path, ..)
            | Op::Move(path, _) => path,
        }
    }

    pub fn is_replace(&self) -> bool {
        matches!(self, Op::Replace(_))
    }
}

impl fmt::Debug for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

/// A reference to a path inside a wire batch: inline, or an id assigned on second use.
#[derive(Clone, PartialEq)]
pub enum PathRef {
    Path(Path),
    Id(u64),
}

/// What crosses a boundary: an [`Op`] with interned paths, where `None` reuses the path of the
/// op just before it, plus `["#", id, path]` definitions.
#[derive(Clone, PartialEq)]
pub enum WireOp {
    Replace(Json),
    Set(Option<PathRef>, Json),
    Delete(Option<PathRef>),
    Append(Option<PathRef>, Arc<str>),
    Truncate(Option<PathRef>, u64),
    Splice(Option<PathRef>, u64, u64, Vec<Json>),
    Move(Option<PathRef>, Vec<usize>),
    Define(u64, Path),
}

impl WireOp {
    pub fn is_replace(&self) -> bool {
        matches!(self, WireOp::Replace(_))
    }
}

impl fmt::Debug for WireOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

/// Whether a batch begins with a replacement. Flush guarantees `r` is first or absent.
pub fn is_base(ops: &[Op]) -> bool {
    ops.first().is_some_and(Op::is_replace)
}

/// [`is_base`] for wire batches.
pub fn is_wire_base(ops: &[WireOp]) -> bool {
    ops.first().is_some_and(WireOp::is_replace)
}

pub(crate) fn unsafe_segment(segment: &Seg) -> ChordError {
    ChordError::UnsafePath {
        segment: match segment {
            Seg::Key(key) => key.to_string(),
            Seg::Index(index) => index.to_string(),
        },
    }
}

/// Reject reserved keys anywhere in a path.
pub fn assert_safe_path(path: &[Seg]) -> Result<(), ChordError> {
    for segment in path {
        if let Seg::Key(key) = segment
            && is_reserved(key)
        {
            return Err(unsafe_segment(segment));
        }
    }
    Ok(())
}
