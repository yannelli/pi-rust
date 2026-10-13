//! Tuple encoding of [`Op`] and [`WireOp`], with upstream's structural validation and messages.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{Op, Path, PathRef, Seg, WireOp, is_reserved, path_to_json};
use crate::error::{ChordError, ChordResult};
use crate::json::{Json, format_js_number};

fn type_error(message: &str) -> ChordError {
    ChordError::type_error(message)
}

/// JavaScript `String(value)` for error messages.
pub(crate) fn js_string(value: &Json) -> String {
    match value {
        Json::String(text) => text.to_string(),
        Json::Number(number) => format_js_number(number.as_f64()),
        Json::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Json::Object(_) => "[object Object]".into(),
        other => other.to_string(),
    }
}

fn parse_segment(segment: &Json) -> ChordResult<Seg> {
    match segment {
        Json::String(key) if is_reserved(key) => Err(ChordError::UnsafePath { segment: key.to_string() }),
        Json::String(key) => Ok(Seg::Key(key.clone())),
        Json::Number(number) => match number.as_u64() {
            Some(index) => Ok(Seg::Index(index)),
            None => Err(ChordError::UnsafePath { segment: js_string(segment) }),
        },
        other => Err(ChordError::UnsafePath { segment: js_string(other) }),
    }
}

/// Parse and check a path array: object keys must not be reserved and indices must be valid.
pub(crate) fn parse_path(value: &Json, non_empty: bool) -> ChordResult<Path> {
    let items = value.as_array().ok_or_else(|| type_error("path is not an array"))?;
    if non_empty && items.is_empty() {
        return Err(type_error("path is empty"));
    }
    items.iter().map(parse_segment).collect()
}

fn non_negative_integer(value: &Json) -> Option<u64> {
    match value {
        Json::Number(number) if number.is_integer() && number.as_f64() >= 0.0 => {
            Some(number.as_u64().unwrap_or(u64::MAX))
        }
        _ => None,
    }
}

fn parse_permutation(value: &Json) -> ChordResult<Vec<usize>> {
    let items = value.as_array().ok_or_else(|| type_error("m permutation is not an array"))?;
    let mut seen = vec![false; items.len()];
    let mut permutation = Vec::with_capacity(items.len());
    for item in items.iter() {
        let index = non_negative_integer(item).and_then(|index| usize::try_from(index).ok());
        match index {
            Some(index) if index < items.len() && !seen[index] => {
                seen[index] = true;
                permutation.push(index);
            }
            _ => return Err(type_error("m permutation is not a bijection")),
        }
    }
    Ok(permutation)
}

fn tuple(value: &Json) -> ChordResult<&Vec<Json>> {
    match value.as_array() {
        Some(items) if !items.is_empty() => Ok(items),
        _ => Err(type_error("op is not a tuple")),
    }
}

fn unknown_verb(verb: &Json) -> ChordError {
    ChordError::type_error(format!("unknown op verb: {}", js_string(verb)))
}

/// Parse a decoded operation: verb, arity, path safety, and payload shape (not payload strictness).
pub fn parse_op(value: &Json) -> ChordResult<Op> {
    let op = tuple(value)?;
    match op[0].as_str() {
        Some("r") => {
            if op.len() != 2 {
                return Err(type_error("r arity"));
            }
            Ok(Op::Replace(op[1].clone()))
        }
        Some("s") => {
            if op.len() != 3 {
                return Err(type_error("s arity"));
            }
            Ok(Op::Set(parse_path(&op[1], true)?, op[2].clone()))
        }
        Some("d") => {
            if op.len() != 2 {
                return Err(type_error("d arity"));
            }
            Ok(Op::Delete(parse_path(&op[1], true)?))
        }
        Some("a") => {
            let text = match (op.len(), op.get(2)) {
                (3, Some(Json::String(text))) => text.clone(),
                _ => return Err(type_error("a shape")),
            };
            Ok(Op::Append(parse_path(&op[1], true)?, text))
        }
        Some("t") => {
            let count = match op.len() {
                3 => non_negative_integer(&op[2]).ok_or_else(|| type_error("t shape"))?,
                _ => return Err(type_error("t shape")),
            };
            Ok(Op::Truncate(parse_path(&op[1], true)?, count))
        }
        Some("p") => {
            if op.len() != 5 {
                return Err(type_error("p arity"));
            }
            let path = parse_path(&op[1], false)?;
            let index = non_negative_integer(&op[2]).ok_or_else(|| type_error("p index"))?;
            let remove = non_negative_integer(&op[3]).ok_or_else(|| type_error("p remove"))?;
            let items = op[4].as_array().ok_or_else(|| type_error("p items"))?;
            Ok(Op::Splice(path, index, remove, items.clone()))
        }
        Some("m") => {
            if op.len() != 3 {
                return Err(type_error("m arity"));
            }
            let path = parse_path(&op[1], false)?;
            Ok(Op::Move(path, parse_permutation(&op[2])?))
        }
        _ => Err(unknown_verb(&op[0])),
    }
}

/// Check a decoded operation without keeping it.
pub fn assert_valid_op(value: &Json) -> ChordResult<()> {
    parse_op(value).map(|_| ())
}

/// Parse an array of decoded operations.
pub fn parse_ops(value: &Json) -> ChordResult<Vec<Op>> {
    let items = value.as_array().ok_or_else(|| type_error("ops is not an array"))?;
    items.iter().map(parse_op).collect()
}

fn parse_ref(value: &Json) -> ChordResult<PathRef> {
    match value {
        Json::Number(_) => non_negative_integer(value).map(PathRef::Id).ok_or_else(|| type_error("bad path id")),
        Json::Array(_) => Ok(PathRef::Path(parse_path(value, false)?)),
        _ => Err(type_error("path is not an array")),
    }
}

/// Parse a wire operation, where path ids and short forms are legal.
pub fn parse_wire_op(value: &Json) -> ChordResult<WireOp> {
    let op = tuple(value)?;
    let len = op.len();
    match op[0].as_str() {
        Some("r") if len == 2 => Ok(WireOp::Replace(op[1].clone())),
        Some("r") => Err(type_error("r arity")),
        Some("s") => match len {
            3 => Ok(WireOp::Set(Some(parse_ref(&op[1])?), op[2].clone())),
            2 => Ok(WireOp::Set(None, op[1].clone())),
            _ => Err(type_error("s arity")),
        },
        Some("d") => match len {
            2 => Ok(WireOp::Delete(Some(parse_ref(&op[1])?))),
            1 => Ok(WireOp::Delete(None)),
            _ => Err(type_error("d arity")),
        },
        Some("a") => {
            let (reference, text) = match len {
                3 => (Some(parse_ref(&op[1])?), &op[2]),
                2 => (None, &op[1]),
                _ => return Err(type_error("a arity")),
            };
            let text = text.as_str().ok_or_else(|| type_error("a value"))?;
            Ok(WireOp::Append(reference, text.into()))
        }
        Some("t") => {
            let (reference, count) = match len {
                3 => (Some(parse_ref(&op[1])?), &op[2]),
                2 => (None, &op[1]),
                _ => return Err(type_error("t arity")),
            };
            Ok(WireOp::Truncate(reference, non_negative_integer(count).ok_or_else(|| type_error("t count"))?))
        }
        Some("p") => {
            let (reference, rest) = match len {
                5 => (Some(parse_ref(&op[1])?), &op[2..]),
                4 => (None, &op[1..]),
                _ => return Err(type_error("p arity")),
            };
            let index = non_negative_integer(&rest[0]).ok_or_else(|| type_error("p index"))?;
            let remove = non_negative_integer(&rest[1]).ok_or_else(|| type_error("p remove"))?;
            let items = rest[2].as_array().ok_or_else(|| type_error("p items"))?;
            Ok(WireOp::Splice(reference, index, remove, items.clone()))
        }
        Some("m") => {
            let reference = match len {
                3 => Some(parse_ref(&op[1])?),
                2 => None,
                _ => return Err(type_error("m arity")),
            };
            Ok(WireOp::Move(reference, parse_permutation(&op[len - 1])?))
        }
        Some("#") => {
            let id = match (len, op.get(1).and_then(non_negative_integer), op.get(2)) {
                (3, Some(id), Some(Json::Array(_))) => id,
                _ => return Err(type_error("# shape")),
            };
            Ok(WireOp::Define(id, parse_path(&op[2], false)?))
        }
        _ => Err(unknown_verb(&op[0])),
    }
}

/// Check a wire operation without keeping it.
pub fn assert_valid_wire_op(value: &Json) -> ChordResult<()> {
    parse_wire_op(value).map(|_| ())
}

/// Parse an array of wire operations.
pub fn parse_wire_ops(value: &Json) -> ChordResult<Vec<WireOp>> {
    let items = value.as_array().ok_or_else(|| type_error("ops is not an array"))?;
    items.iter().map(parse_wire_op).collect()
}

fn permutation_json(permutation: &[usize]) -> Json {
    Json::array(permutation.iter().map(|index| Json::from(*index)).collect())
}

fn tuple_json(items: Vec<Json>) -> Json {
    Json::array(items)
}

impl Op {
    /// The tuple form, for example `["s", ["count"], 2]`.
    pub fn to_json(&self) -> Json {
        match self {
            Op::Replace(value) => tuple_json(vec!["r".into(), value.clone()]),
            Op::Set(path, value) => tuple_json(vec!["s".into(), path_to_json(path), value.clone()]),
            Op::Delete(path) => tuple_json(vec!["d".into(), path_to_json(path)]),
            Op::Append(path, text) => tuple_json(vec!["a".into(), path_to_json(path), text.clone().into()]),
            Op::Truncate(path, count) => tuple_json(vec!["t".into(), path_to_json(path), (*count).into()]),
            Op::Splice(path, index, remove, items) => tuple_json(vec![
                "p".into(),
                path_to_json(path),
                (*index).into(),
                (*remove).into(),
                Json::array(items.clone()),
            ]),
            Op::Move(path, permutation) => {
                tuple_json(vec!["m".into(), path_to_json(path), permutation_json(permutation)])
            }
        }
    }
}

fn with_ref(verb: &str, reference: &Option<PathRef>, rest: Vec<Json>) -> Json {
    let mut items = vec![Json::from(verb)];
    match reference {
        Some(PathRef::Path(path)) => items.push(path_to_json(path)),
        Some(PathRef::Id(id)) => items.push((*id).into()),
        None => {}
    }
    items.extend(rest);
    Json::array(items)
}

impl WireOp {
    /// The tuple form, for example `["a", 0, "x"]` or the short `["a", "x"]`.
    pub fn to_json(&self) -> Json {
        match self {
            WireOp::Replace(value) => tuple_json(vec!["r".into(), value.clone()]),
            WireOp::Set(reference, value) => with_ref("s", reference, vec![value.clone()]),
            WireOp::Delete(reference) => with_ref("d", reference, vec![]),
            WireOp::Append(reference, text) => with_ref("a", reference, vec![text.clone().into()]),
            WireOp::Truncate(reference, count) => with_ref("t", reference, vec![(*count).into()]),
            WireOp::Splice(reference, index, remove, items) => {
                with_ref("p", reference, vec![(*index).into(), (*remove).into(), Json::array(items.clone())])
            }
            WireOp::Move(reference, permutation) => with_ref("m", reference, vec![permutation_json(permutation)]),
            WireOp::Define(id, path) => tuple_json(vec!["#".into(), (*id).into(), path_to_json(path)]),
        }
    }
}

macro_rules! tuple_serde {
    ($ty:ty, $parse:ident) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.to_json().serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = Json::deserialize(deserializer)?;
                $parse(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}
tuple_serde!(Op, parse_op);
tuple_serde!(WireOp, parse_wire_op);
