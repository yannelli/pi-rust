//! Leaf-by-leaf assignment of JSON into a document change (upstream `harness/json.ts`).

use std::collections::HashMap;

use pi_chord::delta::{Change, Seg};
use pi_chord::{ChordError, ChordResult, Json};

/// How [`assign_json`] writes one value instead of assigning it leaf by leaf.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AssignPlan {
    /// The string grew by this text since it was last assigned: append it, when the lengths agree.
    Append(String),
    /// Leave the value as it is; the caller writes it.
    Keep,
    /// Plans for some members or items of a container.
    Fields(HashMap<Seg, AssignPlan>),
}

/// An append [`assign_json`] makes after the assignment: the path of the object, its key, and the text.
type Append = (Vec<Seg>, Seg, String);

/// Assign `value` at `key` of the container at `path` in `change`'s draft, leaf by leaf, so a growing partial does not
/// store and publish the whole message on every commit. Where `plan` says what a string member grew by, and the
/// draft's string plus that text has the new string's length, the text is appended with `Change::append_text()`.
pub(crate) fn assign_json(
    change: &mut Change,
    path: &[Seg],
    key: Seg,
    value: Json,
    plan: Option<&AssignPlan>,
) -> ChordResult<()> {
    let mut appends = Vec::new();
    let target = node_mut(change.state()?, path).ok_or_else(|| ChordError::type_error("assignJson() needs a draft"))?;
    let mut at = path.to_vec();
    assign(target, &key, value, plan, &mut at, &mut appends);
    for (path, key, text) in appends {
        let Seg::Key(key) = key else { continue };
        change.append_text(&path, &key, &text)?;
    }
    Ok(())
}

/// The value at `path` below `root`.
pub(crate) fn node_mut<'a>(root: &'a mut Json, path: &[Seg]) -> Option<&'a mut Json> {
    let mut node = root;
    for segment in path {
        node = slot_mut(node, segment)?;
    }
    Some(node)
}

fn slot_mut<'a>(container: &'a mut Json, key: &Seg) -> Option<&'a mut Json> {
    match key {
        Seg::Key(name) => container.get_mut(name),
        Seg::Index(index) => container.as_array_mut()?.get_mut(*index as usize),
    }
}

fn is_container(value: &Json) -> bool {
    matches!(value, Json::Object(_) | Json::Array(_))
}

fn assign(
    target: &mut Json,
    key: &Seg,
    value: Json,
    plan: Option<&AssignPlan>,
    path: &mut Vec<Seg>,
    appends: &mut Vec<Append>,
) {
    if matches!(plan, Some(AssignPlan::Keep)) {
        return;
    }
    let fields = match plan {
        Some(AssignPlan::Fields(fields)) => Some(fields),
        _ => None,
    };
    // `append_text()` takes object members; a string in an array is assigned.
    let append = match plan {
        Some(AssignPlan::Append(text)) if matches!(target, Json::Object(_)) => Some(text),
        _ => None,
    };
    let Some(current) = slot_mut(target, key) else {
        if let (Seg::Key(name), Some(map)) = (key, target.as_object_mut()) {
            map.insert(name.clone(), value);
        }
        return;
    };
    let fits = |before: &str, after: &str| append.is_some_and(|text| before.len() + text.len() == after.len());
    let step = match (&*current, &value) {
        (Json::Object(_), Json::Object(_)) => Step::Members,
        (Json::Array(items), Json::Array(next)) if items.len() <= next.len() => Step::Items,
        (Json::String(before), Json::String(after)) if fits(before, after) => Step::Append,
        (current, value) if is_container(current) || is_container(value) || current != value => Step::Set,
        _ => Step::Same,
    };
    path.push(key.clone());
    match (step, &value) {
        (Step::Members, Json::Object(next)) => {
            if let Some(map) = current.as_object_mut() {
                let stale: Vec<_> = map.keys().filter(|name| next.get(name).is_none()).cloned().collect();
                for name in stale {
                    map.remove(&name);
                }
            }
            for (name, child) in next.iter() {
                let name = Seg::Key(name.clone());
                assign(current, &name, child.clone(), fields.and_then(|fields| fields.get(&name)), path, appends);
            }
        }
        (Step::Items, Json::Array(next)) => {
            let known = current.as_array_mut().map_or(0, |items| items.len());
            for (index, item) in next.iter().enumerate() {
                if index < known {
                    let index = Seg::Index(index as u64);
                    assign(current, &index, item.clone(), fields.and_then(|fields| fields.get(&index)), path, appends);
                } else if let Some(items) = current.as_array_mut() {
                    items.push(item.clone());
                }
            }
        }
        (Step::Append, _) => {
            path.pop();
            appends.push((path.clone(), key.clone(), append.cloned().unwrap_or_default()));
            return;
        }
        (Step::Set, _) => *current = value,
        _ => {}
    }
    path.pop();
}

/// What [`assign`] does with one slot.
enum Step {
    Members,
    Items,
    Append,
    Set,
    Same,
}
