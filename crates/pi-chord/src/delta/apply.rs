//! Appliers. Values are copy-on-write, so the mutable and immutable forms share one implementation:
//! containers shared with other holders are copied once per call and never changed in place.

use std::sync::Arc;

use super::{Op, Seg, assert_safe_path, path_to_json, unsafe_segment};
use crate::error::{ChordError, ChordResult};
use crate::json::{Json, utf16_offset};

pub(crate) fn path_error(path: &[Seg]) -> ChordError {
    ChordError::Path { path: path_to_json(path).to_string() }
}

/// Structural checks a typed op cannot express: non-empty paths, safe keys, and bijections.
pub(crate) fn assert_typed_op(op: &Op) -> ChordResult<()> {
    match op {
        Op::Replace(_) => Ok(()),
        Op::Set(path, _) | Op::Delete(path) | Op::Append(path, _) | Op::Truncate(path, _) => {
            if path.is_empty() {
                return Err(ChordError::type_error("path is empty"));
            }
            assert_safe_path(path)
        }
        Op::Splice(path, ..) => assert_safe_path(path),
        Op::Move(path, permutation) => {
            assert_safe_path(path)?;
            let mut seen = vec![false; permutation.len()];
            for &index in permutation {
                if index >= permutation.len() || seen[index] {
                    return Err(ChordError::type_error("m permutation is not a bijection"));
                }
                seen[index] = true;
            }
            Ok(())
        }
    }
}

/// Apply decoded ops to an owned value and return the result; `r` replaces it outright.
pub fn apply(target: Option<Json>, ops: &[Op]) -> ChordResult<Json> {
    let mut root = target;
    for op in ops {
        apply_one(&mut root, op)?;
    }
    root.ok_or_else(|| path_error(&[]))
}

/// Apply one decoded batch without changing `target` or anything it shares.
pub fn apply_immutable(target: Option<&Json>, ops: &[Op]) -> ChordResult<Json> {
    apply(target.cloned(), ops)
}

/// Apply ordered batches as one final-result-only replay.
pub fn apply_immutable_batches<'a>(
    target: Option<&Json>,
    batches: impl IntoIterator<Item = &'a [Op]>,
) -> ChordResult<Json> {
    let mut root = target.cloned();
    for batch in batches {
        for op in batch {
            apply_one(&mut root, op)?;
        }
    }
    root.ok_or_else(|| path_error(&[]))
}

fn object_key(segment: &Seg) -> Arc<str> {
    match segment {
        Seg::Key(key) => key.clone(),
        Seg::Index(index) => index.to_string().into(),
    }
}

/// Walk to the container at `path`, copying shared containers on the way.
fn resolve_mut<'a>(root: &'a mut Json, path: &[Seg]) -> ChordResult<&'a mut Json> {
    let mut node = root;
    for segment in path {
        node = match node {
            Json::Array(items) => {
                let Seg::Index(index) = segment else {
                    return Err(unsafe_segment(segment));
                };
                let items = Arc::make_mut(items);
                usize::try_from(*index).ok().and_then(|index| items.get_mut(index)).ok_or_else(|| path_error(path))?
            }
            Json::Object(map) => Arc::make_mut(map).get_mut(&object_key(segment)).ok_or_else(|| path_error(path))?,
            _ => return Err(path_error(path)),
        };
    }
    if node.is_container() { Ok(node) } else { Err(path_error(path)) }
}

fn apply_one(root: &mut Option<Json>, op: &Op) -> ChordResult<()> {
    assert_typed_op(op)?;
    if let Op::Replace(value) = op {
        *root = Some(value.clone());
        return Ok(());
    }
    let path = op.path();
    let Some(value) = root.as_mut() else {
        return Err(path_error(path));
    };
    match op {
        Op::Splice(_, index, remove, items) => {
            let target = resolve_mut(value, path)?.as_array_mut().ok_or_else(|| path_error(path))?;
            let start = usize::try_from(*index).unwrap_or(usize::MAX).min(target.len());
            let end = start.saturating_add(usize::try_from(*remove).unwrap_or(usize::MAX)).min(target.len());
            target.splice(start..end, items.iter().cloned());
            Ok(())
        }
        Op::Move(_, permutation) => {
            let target = resolve_mut(value, path)?.as_array_mut().ok_or_else(|| path_error(path))?;
            if target.len() != permutation.len() {
                return Err(path_error(path));
            }
            let previous = std::mem::take(target);
            target.extend(permutation.iter().map(|&index| previous[index].clone()));
            Ok(())
        }
        _ => apply_leaf(value, op),
    }
}

fn apply_leaf(root: &mut Json, op: &Op) -> ChordResult<()> {
    let path = op.path();
    let (key, parent_path) = path.split_last().ok_or_else(|| ChordError::type_error("path is empty"))?;
    let parent = resolve_mut(root, parent_path)?;
    match parent {
        Json::Array(items) => {
            let Seg::Index(index) = key else {
                return Err(unsafe_segment(key));
            };
            let items = Arc::make_mut(items);
            let index = usize::try_from(*index).ok().filter(|index| *index <= items.len());
            let index = index.ok_or_else(|| unsafe_segment(key))?;
            match op {
                Op::Set(_, value) => {
                    if index == items.len() {
                        items.push(value.clone());
                    } else {
                        items[index] = value.clone();
                    }
                }
                Op::Delete(_) => {
                    if index >= items.len() {
                        return Err(path_error(path));
                    }
                    items.remove(index);
                }
                _ => {
                    let current = items.get_mut(index).ok_or_else(|| path_error(path))?;
                    edit_string(current, op, path)?;
                }
            }
        }
        Json::Object(map) => {
            let map = Arc::make_mut(map);
            let key = object_key(key);
            match op {
                Op::Set(_, value) => {
                    map.insert(key, value.clone());
                }
                Op::Delete(_) => {
                    map.remove(&key);
                }
                _ => {
                    let current = map.get_mut(&key).ok_or_else(|| path_error(path))?;
                    edit_string(current, op, path)?;
                }
            }
        }
        _ => return Err(path_error(parent_path)),
    }
    Ok(())
}

fn edit_string(current: &mut Json, op: &Op, path: &[Seg]) -> ChordResult<()> {
    let Json::String(text) = current else {
        return Err(path_error(path));
    };
    match op {
        Op::Append(_, suffix) => {
            let mut next = String::with_capacity(text.len() + suffix.len());
            next.push_str(text);
            next.push_str(suffix);
            *text = next.into();
        }
        Op::Truncate(_, count) => {
            let units = usize::try_from(*count).unwrap_or(usize::MAX);
            let offset = utf16_offset(text, units)
                .ok_or_else(|| ChordError::type_error("t count splits a UTF-16 surrogate pair"))?;
            *text = text[offset..].into();
        }
        _ => return Err(path_error(path)),
    }
    Ok(())
}
