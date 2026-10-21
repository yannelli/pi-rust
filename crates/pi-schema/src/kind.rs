//! TypeBox kinds read from JSON Schema keywords, and TypeBox's union priority order. TypeBox's
//! `Default` and `Clean` dispatch on hidden `~kind` markers that JSON does not carry, so the kind
//! comes from the keywords TypeBox writes for each kind instead.

use serde_json::{Map, Value};

use crate::check::json_equal;

/// The TypeBox kinds that `Default` and `Clean` treat structurally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Object,
    Record,
    Array,
    Tuple,
    Union,
    Intersect,
    Other,
}

pub(crate) fn kind(schema: &Value) -> Kind {
    let Value::Object(map) = schema else { return Kind::Other };
    if matches!(map.get("anyOf"), Some(Value::Array(_))) {
        return Kind::Union;
    }
    if matches!(map.get("allOf"), Some(Value::Array(_))) {
        return Kind::Intersect;
    }
    match map.get("type").and_then(Value::as_str) {
        Some("object") if map.get("properties").is_some_and(Value::is_object) => Kind::Object,
        Some("object") if map.get("patternProperties").is_some_and(Value::is_object) => Kind::Record,
        Some("array") => match map.get("items") {
            Some(Value::Array(_)) => Kind::Tuple,
            Some(Value::Object(_) | Value::Bool(_)) => Kind::Array,
            _ => Kind::Other,
        },
        _ => Kind::Other,
    }
}

/// The record's key pattern and value schema: its first `patternProperties` entry.
pub(crate) fn record_entry(schema: &Value) -> Option<(&str, &Value)> {
    schema.get("patternProperties")?.as_object()?.iter().next().map(|(pattern, value)| (pattern.as_str(), value))
}

/// An intersection of objects as one object schema (TypeBox `Evaluate`); `None` for other intersections.
pub(crate) fn evaluate_intersect(schema: &Value) -> Option<Value> {
    let branches = schema.get("allOf")?.as_array()?;
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for branch in branches {
        let branch = match kind(branch) {
            Kind::Object => branch.clone(),
            Kind::Intersect => evaluate_intersect(branch)?,
            _ => return None,
        };
        for (key, property) in branch.get("properties").and_then(Value::as_object).into_iter().flatten() {
            let merged = match properties.remove(key) {
                Some(existing) => serde_json::json!({ "allOf": [existing, property] }),
                None => property.clone(),
            };
            properties.insert(key.clone(), merged);
        }
        for key in branch.get("required").and_then(Value::as_array).into_iter().flatten() {
            if !required.contains(key) {
                required.push(key.clone());
            }
        }
    }
    let mut object = Map::new();
    object.insert("type".into(), "object".into());
    if !required.is_empty() {
        object.insert("required".into(), Value::Array(required));
    }
    object.insert("properties".into(), Value::Object(properties));
    if let Some(unevaluated) = schema.get("unevaluatedProperties") {
        object.insert("additionalProperties".into(), unevaluated.clone());
    }
    Some(Value::Object(object))
}

fn types(schema: &Map<String, Value>) -> Option<Vec<&str>> {
    if let Some(constant) = schema.get("const") {
        return Some(vec![json_type(constant)]);
    }
    match schema.get("type")? {
        Value::String(kind) => Some(vec![kind.as_str()]),
        Value::Array(kinds) => Some(kinds.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.as_f64().is_some_and(|value| value.fract() == 0.0) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

const STRUCTURE: [&str; 10] =
    ["type", "const", "enum", "anyOf", "allOf", "not", "properties", "patternProperties", "items", "$ref"];

/// Whether every value of `left` is a value of `right`, as TypeBox's type-level `Extends` decides it:
/// structure only, constraints such as `minLength` ignored.
pub(crate) fn extends(left: &Value, right: &Value) -> bool {
    let (Value::Object(l), Value::Object(r)) = (left, right) else {
        return matches!(right, Value::Bool(true)) || left == right;
    };
    if !STRUCTURE.iter().any(|key| r.contains_key(*key)) {
        return true;
    }
    if !STRUCTURE.iter().any(|key| l.contains_key(*key)) {
        return false;
    }
    if let Some(Value::Array(branches)) = l.get("anyOf") {
        return branches.iter().all(|branch| extends(branch, right));
    }
    if let Some(Value::Array(branches)) = r.get("anyOf") {
        return branches.iter().any(|branch| extends(left, branch));
    }
    if let Some(Value::Array(branches)) = l.get("allOf") {
        return branches.iter().any(|branch| extends(branch, right));
    }
    if let Some(Value::Array(branches)) = r.get("allOf") {
        return branches.iter().all(|branch| extends(left, branch));
    }
    if let Some(expected) = r.get("const") {
        return l.get("const").is_some_and(|actual| json_equal(actual, expected));
    }
    if let Some(Value::Array(allowed)) = r.get("enum") {
        let values: Vec<&Value> = match (l.get("const"), l.get("enum")) {
            (Some(constant), _) => vec![constant],
            (None, Some(Value::Array(values))) => values.iter().collect(),
            _ => return false,
        };
        return values.iter().all(|value| allowed.iter().any(|option| json_equal(option, value)));
    }
    let (Some(left_types), Some(right_types)) = (types(l), types(r)) else { return false };
    let fits = |kind: &&str| right_types.contains(kind) || (*kind == "integer" && right_types.contains(&"number"));
    if !left_types.iter().all(fits) {
        return false;
    }
    if l.contains_key("const") || l.contains_key("enum") {
        return true;
    }
    match left_types.first().copied() {
        Some("object") => object_extends(l, r),
        Some("array") => array_extends(l, r),
        _ => true,
    }
}

fn required(schema: &Map<String, Value>, key: &str) -> bool {
    schema.get("required").and_then(Value::as_array).is_some_and(|keys| keys.iter().any(|item| item == key))
}

fn first_pattern(schema: &Map<String, Value>) -> Option<&Value> {
    schema.get("patternProperties")?.as_object()?.values().next()
}

fn object_extends(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    let empty = Map::new();
    let left_properties = left.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    let right_properties = right.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    for (key, property) in right_properties {
        match left_properties.get(key) {
            Some(own) if extends(own, property) && (required(left, key) || !required(right, key)) => {}
            None if !required(right, key) => {}
            _ => return false,
        }
    }
    match first_pattern(right) {
        Some(value) => {
            left_properties.values().all(|property| extends(property, value))
                && first_pattern(left).is_none_or(|own| extends(own, value))
        }
        None => true,
    }
}

fn array_extends(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    match (left.get("items"), right.get("items")) {
        (Some(Value::Array(own)), Some(Value::Array(other))) => {
            own.len() == other.len() && own.iter().zip(other).all(|(own, other)| extends(own, other))
        }
        (Some(Value::Array(own)), Some(other)) => own.iter().all(|own| extends(own, other)),
        (Some(_), Some(Value::Array(_))) => false,
        (Some(own), Some(other)) => extends(own, other),
        (_, None) => true,
        (None, Some(other)) => extends(&Value::Object(Map::new()), other),
    }
}

/// TypeBox `Comparer(item, placed) === 1`: `placed` is strictly inside `item`, or the two are disjoint.
/// Both mean that `item` does not extend `placed`.
fn goes_after(item: &Value, placed: &Value) -> bool {
    !extends(item, placed)
}

/// TypeBox `Priority`: an insertion sort from the narrowest to the broadest type.
fn priority(types: Vec<Value>) -> Vec<Value> {
    let mut sorted: Vec<Value> = Vec::with_capacity(types.len());
    for item in types {
        let position = sorted.iter().position(|placed| !goes_after(&item, placed)).unwrap_or(sorted.len());
        sorted.insert(position, item);
    }
    sorted
}

/// TypeBox `UnionPrioritySort`: every union's branches from narrowest to broadest, recursively.
pub(crate) fn union_priority_sort(schema: &Value) -> Value {
    let mut next = schema.clone();
    let schema_kind = kind(schema);
    let Value::Object(map) = &mut next else { return next };
    let sort_all = |items: &[Value]| items.iter().map(union_priority_sort).collect::<Vec<_>>();
    match schema_kind {
        Kind::Array => {
            if let Some(items) = map.get_mut("items") {
                *items = union_priority_sort(items);
            }
        }
        Kind::Tuple | Kind::Intersect => {
            let keyword = if schema_kind == Kind::Tuple { "items" } else { "allOf" };
            if let Some(Value::Array(items)) = map.get_mut(keyword) {
                *items = sort_all(items);
            }
        }
        Kind::Union => {
            if let Some(Value::Array(items)) = map.get_mut("anyOf") {
                *items = priority(sort_all(items));
            }
        }
        Kind::Object => {
            if let Some(Value::Object(properties)) = map.get_mut("properties") {
                properties.values_mut().for_each(|property| *property = union_priority_sort(property));
            }
        }
        Kind::Record => {
            if let Some(Value::Object(patterns)) = map.get_mut("patternProperties")
                && let Some(value) = patterns.values_mut().next()
            {
                *value = union_priority_sort(value);
            }
        }
        Kind::Other => {}
    }
    next
}
