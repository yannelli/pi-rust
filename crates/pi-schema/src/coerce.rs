//! Coercion (upstream pi-schema `coerce.ts`): turns a loosely typed value, such as a model's tool
//! call arguments, into the value a schema most likely meant, before it is checked. Three passes:
//! 1. Optional nulls: a `null` in an optional property whose schema rejects null is dropped.
//! 2. TypeBox 1.3.27's `Value.Convert` rules keyed on JSON Schema structure: `null` becomes
//!    `"null"` for a string, `"3.5"` becomes 3 for an integer, a single value becomes a one-item
//!    array, and a union takes the first branch whose conversion passes.
//! 3. Primitive coercion by `type` (also type arrays), as in Ajv's `coerceTypes`.
//!
//! A value that cannot be coerced stays as it is; the check then reports it. Each pass returns
//! `None` when nothing changes.

use std::collections::HashMap;

use regex::Regex;
use serde_json::{Map, Value};

use super::check::{Checker, matches_type};
use super::convert::{
    changed, coerce_primitive, convert_literal, number_value, try_boolean, try_null, try_number, try_string,
};
use super::explain::{EMPTY, js_json, strict_equal};

/// A value as a re-entry point compares it: objects and arrays by address, others by value.
#[derive(Clone, PartialEq, Eq, Hash)]
enum ValueKey {
    Address(usize),
    Primitive(String),
}

fn value_key(value: &Value) -> ValueKey {
    match value {
        Value::Object(_) | Value::Array(_) => ValueKey::Address(std::ptr::from_ref(value).addr()),
        other => ValueKey::Primitive(js_json(other)),
    }
}

fn address(node: &Value) -> usize {
    std::ptr::from_ref(node).addr()
}

pub(crate) struct Coercer<'a> {
    root: &'a Value,
    checker: Checker<'a>,
    /// The (schema node, value) pairs pass 2 converts through a re-entry point (a `$ref` or a
    /// wrap into an array). A pair already here is a cycle and leaves the value unchanged.
    entered: Vec<(usize, ValueKey)>,
    /// How often a re-entry was cut short; results computed across one are not memoized.
    cuts: usize,
    /// Results of `$ref` targets by node and value: a recursive union whose branches all convert
    /// the same children would otherwise convert them once per branch.
    memo: HashMap<(usize, String), Option<Value>>,
}

impl<'a> Coercer<'a> {
    pub fn new(root: &'a Value) -> Self {
        let checker = Checker::new(root).rejecting_unresolved_refs();
        Self { root, checker, entered: Vec::new(), cuts: 0, memo: HashMap::new() }
    }

    /// `value` coerced; a copy, `value` is never modified.
    pub fn coerce(mut self, value: &Value) -> Value {
        let without_nulls = self.drop_optional_nulls(value, self.root);
        let current = without_nulls.as_ref().unwrap_or(value);
        let converted = self.convert(self.root, current);
        let current = converted.as_ref().unwrap_or(current);
        let typed = self.coerce_types(current, self.root);
        typed.or(converted).or(without_nulls).unwrap_or_else(|| value.clone())
    }

    fn passes(&self, node: &Value, value: &Value) -> bool {
        self.checker.is_valid(value, node)
    }

    // Pass 1: optional nulls

    fn drop_optional_nulls(&self, value: &Value, schema: &Value) -> Option<Value> {
        let Value::Object(schema) = schema else { return None };
        let object = match value {
            Value::Array(items) => {
                return map_items(items, |item, index| {
                    item_schema_at(schema, index).and_then(|item_schema| self.drop_optional_nulls(item, item_schema))
                });
            }
            Value::Object(object) => object,
            _ => return None,
        };
        let members = Members::of(schema);
        let declares_properties = schema.get("properties").is_some_and(Value::is_object);
        if members.patterns.is_empty() && members.additional.is_none() && !declares_properties {
            return None;
        }
        let required: Vec<&str> =
            schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let mut out: Option<Map<String, Value>> = None;
        for (key, property) in members.properties {
            let Some(item) = object.get(key) else { continue };
            let is_ref = property.get("$ref").is_some_and(Value::is_string);
            if item.is_null() && !required.contains(&key.as_str()) && !is_ref && !self.passes(property, &Value::Null) {
                out.get_or_insert_with(|| object.clone()).shift_remove(key);
                continue;
            }
            set(&mut out, object, key, self.drop_optional_nulls(item, property));
        }
        // Pattern and additional property values are walked for their own optional nulls; their
        // keys are not optional properties, so they stay.
        if !members.patterns.is_empty() || members.additional.is_some() {
            for (key, item) in object {
                let mut matched = members.properties.contains_key(key);
                for (pattern, item_schema) in &members.patterns {
                    if !pattern.is_match(key) {
                        continue;
                    }
                    matched = true;
                    let current = out.as_ref().map_or(Some(item), |out| out.get(key));
                    let next = current.and_then(|current| self.drop_optional_nulls(current, item_schema));
                    set(&mut out, object, key, next);
                }
                if let (false, Some(additional)) = (matched, members.additional) {
                    set(&mut out, object, key, self.drop_optional_nulls(item, additional));
                }
            }
        }
        out.map(Value::Object)
    }

    // Pass 2: TypeBox's conversion rules

    fn convert(&mut self, node: &'a Value, value: &Value) -> Option<Value> {
        let Value::Object(schema) = node else { return None };
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let target = self.checker.resolve(reference)?;
            return if target.is_object() { self.reenter(target, value, true) } else { None };
        }
        self.convert_node(node, schema, value)
    }

    /// `node` converted for `value` at a re-entry point: unchanged when that pair is already being
    /// converted.
    fn reenter(&mut self, node: &'a Value, value: &Value, memoize: bool) -> Option<Value> {
        let entry = (address(node), value_key(value));
        if self.entered.contains(&entry) {
            self.cuts += 1;
            return None;
        }
        let memo_key = memoize.then(|| (address(node), js_json(value)));
        if let Some(hit) = memo_key.as_ref().and_then(|key| self.memo.get(key)) {
            return hit.clone();
        }
        let cuts = self.cuts;
        self.entered.push(entry);
        let result = self.convert(node, value);
        self.entered.pop();
        if let Some(key) = memo_key.filter(|_| self.cuts == cuts) {
            self.memo.insert(key, result.clone());
        }
        result
    }

    /// One node's conversion: its first keyword among `const`, `enum`, the unions, `allOf`, and
    /// `type`.
    fn convert_node(&mut self, node: &'a Value, schema: &'a Map<String, Value>, value: &Value) -> Option<Value> {
        if let Some(literal) = schema.get("const") {
            return convert_literal(literal, value);
        }
        if let Some(Value::Array(values)) = schema.get("enum") {
            if values.iter().any(|member| strict_equal(member, value)) {
                return None;
            }
            return values
                .iter()
                .filter_map(|member| convert_literal(member, value))
                .find(|candidate| self.passes(node, candidate));
        }
        // A union or intersection node may also declare members: they convert after the branches.
        for keyword in ["anyOf", "oneOf"] {
            if let Some(Value::Array(branches)) = schema.get(keyword) {
                return self.convert_union(node, schema, branches, value);
            }
        }
        if let Some(Value::Array(members)) = schema.get("allOf") {
            let mut next: Option<Value> = None;
            for member in members {
                if let Some(converted) = self.convert(member, next.as_ref().unwrap_or(value)) {
                    next = Some(converted);
                }
            }
            let current = next.as_ref().unwrap_or(value);
            return self.convert_members(schema, current).or(next);
        }
        match schema.get("type").map(Value::as_str) {
            Some(Some("string")) => changed(value, try_string(value)),
            Some(Some("number")) => changed(value, try_number(value).map(number_value)),
            Some(Some("integer")) => changed(value, try_number(value).map(|number| number_value(number.trunc()))),
            Some(Some("boolean")) => changed(value, try_boolean(value).map(Value::Bool)),
            Some(Some("null")) => changed(value, try_null(value).then_some(Value::Null)),
            Some(Some("array")) => self.convert_array(schema, value),
            Some(Some("object")) => self.convert_object(schema, value),
            None if schema.contains_key("properties") || schema.contains_key("patternProperties") => {
                self.convert_object(schema, value)
            }
            None if schema.contains_key("items") || schema.contains_key("prefixItems") => {
                self.convert_array(schema, value)
            }
            _ => None,
        }
    }

    /// A value one branch accepts stays; else the first branch's conversion the whole union accepts.
    fn convert_union(
        &mut self,
        node: &'a Value,
        schema: &'a Map<String, Value>,
        branches: &'a [Value],
        value: &Value,
    ) -> Option<Value> {
        if branches.iter().any(|branch| self.passes(branch, value)) {
            return self.convert_members(schema, value);
        }
        for branch in branches {
            let converted = self.convert(branch, value);
            let current = converted.as_ref().unwrap_or(value);
            let candidate = self.convert_members(schema, current).or(converted);
            if self.passes(node, candidate.as_ref().unwrap_or(value)) {
                return candidate;
            }
        }
        self.convert_members(schema, value)
    }

    /// The object members of a union or intersection node, when it declares any.
    fn convert_members(&mut self, schema: &'a Map<String, Value>, value: &Value) -> Option<Value> {
        let declares =
            ["properties", "patternProperties", "additionalProperties"].iter().any(|k| schema.contains_key(*k));
        if declares { self.convert_object(schema, value) } else { None }
    }

    fn convert_array(&mut self, schema: &'a Map<String, Value>, value: &Value) -> Option<Value> {
        let items_schema = schema.get("items");
        let prefix = match (items_schema, schema.get("prefixItems")) {
            (Some(Value::Array(tuple)), _) | (_, Some(Value::Array(tuple))) => Some(tuple),
            _ => None,
        };
        if let Some(prefix) = prefix {
            // A tuple converts its items and never wraps a single value.
            let Value::Array(items) = value else { return None };
            let rest = items_schema.filter(|items| !items.is_array());
            return map_items(items, |item, index| {
                prefix.get(index).or(rest).and_then(|item_schema| self.convert(item_schema, item))
            });
        }
        if let Value::Array(items) = value {
            let item_schema = items_schema?;
            return map_items(items, |item, _| self.convert(item_schema, item));
        }
        // The wrapped single value is converted by the item schema with the same value: a re-entry point.
        let item = match items_schema {
            Some(item_schema @ Value::Object(_)) => self.reenter(item_schema, value, false),
            _ => None,
        };
        Some(Value::Array(vec![item.unwrap_or_else(|| value.clone())]))
    }

    fn convert_object(&mut self, schema: &'a Map<String, Value>, value: &Value) -> Option<Value> {
        let Value::Object(object) = value else { return None };
        let members = Members::of(schema);
        let mut out: Option<Map<String, Value>> = None;
        for (key, item) in object {
            let mut next: Option<Value> = None;
            let mut matched = false;
            if let Some(property) = members.properties.get(key) {
                matched = true;
                next = self.convert(property, item);
            }
            for (pattern, item_schema) in &members.patterns {
                if pattern.is_match(key) {
                    matched = true;
                    next = self.convert(item_schema, next.as_ref().unwrap_or(item)).or(next);
                }
            }
            if let (false, Some(additional)) = (matched, members.additional) {
                next = self.convert(additional, item);
            }
            if let Some(next) = next {
                out.get_or_insert_with(|| object.clone()).insert(key.clone(), next);
            }
        }
        out.map(Value::Object)
    }

    // Pass 3: primitive coercion by type

    fn coerce_types(&self, value: &Value, schema: &Value) -> Option<Value> {
        let Value::Object(schema) = schema else { return None };
        let mut next: Option<Value> = None;
        for member in schema.get("allOf").and_then(Value::as_array).into_iter().flatten() {
            next = self.coerce_types(next.as_ref().unwrap_or(value), member).or(next);
        }
        for keyword in ["anyOf", "oneOf"] {
            if let Some(Value::Array(branches)) = schema.get(keyword) {
                next = self.coerce_union(next.as_ref().unwrap_or(value), branches).or(next);
            }
        }
        let kinds: Vec<&str> = match schema.get("type") {
            Some(Value::String(kind)) => vec![kind.as_str()],
            Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let current = next.as_ref().unwrap_or(value);
        let matches_member = kinds.len() > 1 && kinds.iter().any(|kind| matches_type(current, kind));
        if !matches_member && let Some(coerced) = kinds.iter().find_map(|kind| coerce_primitive(current, kind)) {
            next = Some(coerced);
        }
        let current = next.as_ref().unwrap_or(value);
        let coerced_members = match current {
            Value::Object(object) if kinds.contains(&"object") => self.coerce_properties(object, schema),
            Value::Array(items) if kinds.contains(&"array") => map_items(items, |item, index| {
                item_schema_at(schema, index)
                    .filter(|item_schema| item_schema.is_object())
                    .and_then(|item_schema| self.coerce_types(item, item_schema))
            }),
            _ => None,
        };
        coerced_members.or(next)
    }

    fn coerce_union(&self, value: &Value, branches: &[Value]) -> Option<Value> {
        if branches.iter().any(|branch| self.passes(branch, value)) {
            return None;
        }
        branches.iter().find_map(|branch| {
            let coerced = self.coerce_types(value, branch)?;
            self.passes(branch, &coerced).then_some(coerced)
        })
    }

    fn coerce_properties(&self, object: &Map<String, Value>, schema: &Map<String, Value>) -> Option<Value> {
        let members = Members::of(schema);
        let mut out: Option<Map<String, Value>> = None;
        for (key, property) in members.properties {
            if let Some(item) = object.get(key) {
                set(&mut out, object, key, self.coerce_types(item, property));
            }
        }
        for (key, item) in object {
            let mut matched = members.properties.contains_key(key);
            for (pattern, item_schema) in &members.patterns {
                if !pattern.is_match(key) {
                    continue;
                }
                matched = true;
                let current = out.as_ref().and_then(|out| out.get(key)).unwrap_or(item);
                let next = self.coerce_types(current, item_schema);
                set(&mut out, object, key, next);
            }
            if let (false, Some(additional)) = (matched, members.additional) {
                set(&mut out, object, key, self.coerce_types(item, additional));
            }
        }
        out.map(Value::Object)
    }
}

/// The member keywords of an object schema: `properties`, `patternProperties` and an
/// `additionalProperties` schema.
struct Members<'s> {
    properties: &'s Map<String, Value>,
    patterns: Vec<(Regex, &'s Value)>,
    additional: Option<&'s Value>,
}

impl<'s> Members<'s> {
    fn of(schema: &'s Map<String, Value>) -> Self {
        let patterns = match schema.get("patternProperties") {
            Some(Value::Object(patterns)) => {
                patterns.iter().filter_map(|(pattern, item)| Some((Regex::new(pattern).ok()?, item))).collect()
            }
            _ => Vec::new(),
        };
        Self {
            properties: schema.get("properties").and_then(Value::as_object).unwrap_or(&EMPTY),
            patterns,
            additional: schema.get("additionalProperties").filter(|additional| additional.is_object()),
        }
    }
}

/// Put `next` at `key` of the copy, making the copy on the first change.
fn set(out: &mut Option<Map<String, Value>>, object: &Map<String, Value>, key: &str, next: Option<Value>) {
    if let Some(next) = next {
        out.get_or_insert_with(|| object.clone()).insert(key.to_string(), next);
    }
}

/// The schema of an array item at `index`: a tuple's for its prefix, `items` after it.
fn item_schema_at(schema: &Map<String, Value>, index: usize) -> Option<&Value> {
    match (schema.get("items"), schema.get("prefixItems")) {
        (Some(Value::Array(tuple)), _) => tuple.get(index),
        (_, Some(Value::Array(prefix))) if index < prefix.len() => prefix.get(index),
        (items, _) => items,
    }
}

/// `items` mapped by `map`; `None` when every item maps to itself.
fn map_items(items: &[Value], mut map: impl FnMut(&Value, usize) -> Option<Value>) -> Option<Value> {
    let mut out: Option<Vec<Value>> = None;
    for (index, item) in items.iter().enumerate() {
        if let Some(next) = map(item, index) {
            out.get_or_insert_with(|| items.to_vec())[index] = next;
        }
    }
    out.map(Value::Array)
}
