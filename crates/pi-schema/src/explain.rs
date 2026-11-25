//! Errors of a failed value, as pi-schema explains them (upstream `explain.ts`): a walk over the
//! schema that reports each failing keyword at the value it failed on, with a JSON Pointer path.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::check::{BoundCheck, Checker, js_number, json_equal, matches_type};
use super::formats::test_format;

/// One failure of a value: where it is (a JSON Pointer, `""` for the value itself), the failing
/// keyword and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    pub path: String,
    pub keyword: &'static str,
    pub message: String,
}

/// A schema without `properties`.
pub(crate) static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);

/// Enum errors name the allowed values; a long list is cut, so the message stays readable.
const LISTED_VALUES: usize = 20;

pub(crate) struct Explainer<'a> {
    checker: Checker<'a>,
    /// The errors of `$ref` targets for object and array values within one call, by target and
    /// value address: a recursive union explains each branch, and every branch would explain the
    /// same children again, exponentially in the depth.
    memo: HashMap<(usize, usize), (String, Vec<ValidationError>)>,
}

impl<'a> Explainer<'a> {
    pub fn new(root: &'a Value) -> Self {
        Self { checker: Checker::new(root).rejecting_unresolved_refs(), memo: HashMap::new() }
    }

    /// The errors of `value` under `schema`, at least one (`value` failed the check).
    pub fn errors(mut self, schema: &'a Value, value: &Value) -> Vec<ValidationError> {
        let mut out = Vec::new();
        self.walk(schema, value, "", &mut out);
        if out.is_empty() {
            out.push(error("", "schema", "invalid".into()));
        }
        out
    }

    fn ok(&self, node: &Value, value: &Value) -> bool {
        self.checker.is_valid(value, node)
    }

    fn resolve(&self, reference: &str) -> Option<&'a Value> {
        self.checker.resolve(reference)
    }

    fn walk(&mut self, node: &'a Value, value: &Value, path: &str, out: &mut Vec<ValidationError>) {
        let schema = match node {
            Value::Bool(true) => return,
            Value::Bool(false) => return out.push(error(path, "false", "no value is allowed here".into())),
            Value::Object(schema) => schema,
            _ => return,
        };
        if self.ok(node, value) {
            return;
        }
        if let Some(kind) = schema.get("type") {
            let kinds: Vec<&Value> = match kind {
                Value::Array(kinds) => kinds.iter().collect(),
                kind => vec![kind],
            };
            if !kinds.iter().any(|kind| kind.as_str().is_some_and(|kind| matches_type(value, kind))) {
                let names: Vec<String> = kinds.iter().map(|kind| js_string(kind)).collect();
                let message = format!("expected {}, got {}", names.join(" or "), type_name(value));
                return out.push(error(path, "type", message));
            }
        }
        if let Some(Value::Array(values)) = schema.get("enum")
            && !values.iter().any(|allowed| json_equal(allowed, value))
        {
            out.push(error(path, "enum", format!("expected one of {}", list_values(values))));
        }
        if let Some(expected) = schema.get("const")
            && !json_equal(expected, value)
        {
            out.push(error(path, "const", format!("expected {}", js_json(expected))));
        }
        match value {
            Value::String(text) => walk_string(schema, text, path, out),
            Value::Number(number) => walk_number(schema, number.as_f64().unwrap_or_default(), path, out),
            Value::Object(object) => self.walk_object(schema, object, value, path, out),
            Value::Array(items) => self.walk_array(schema, items, path, out),
            _ => {}
        }
        if let Some(Value::Array(members)) = schema.get("allOf") {
            for member in members {
                self.walk(member, value, path, out);
            }
        }
        if let Some(Value::Array(branches)) = schema.get("anyOf") {
            self.walk_union(branches, value, path, out, false);
        }
        if let Some(Value::Array(branches)) = schema.get("oneOf") {
            self.walk_union(branches, value, path, out, true);
        }
        if let Some(negated) = schema.get("not")
            && self.ok(negated, value)
        {
            out.push(error(path, "not", "must not match the schema in not".into()));
        }
        if let Some(condition) = schema.get("if") {
            let branch = if self.ok(condition, value) { schema.get("then") } else { schema.get("else") };
            if let Some(branch) = branch {
                self.walk(branch, value, path, out);
            }
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            self.walk_ref(reference, value, path, out);
        }
    }

    fn walk_ref(&mut self, reference: &str, value: &Value, path: &str, out: &mut Vec<ValidationError>) {
        let Some(target) = self.resolve(reference) else {
            return out.push(error(path, "$ref", format!("unresolved reference {reference}")));
        };
        if !target.is_object() || !(value.is_object() || value.is_array()) {
            return self.walk(target, value, path, out);
        }
        let key = (std::ptr::from_ref(target).addr(), std::ptr::from_ref(value).addr());
        if let Some((known_path, errors)) = self.memo.get(&key)
            && known_path == path
        {
            return out.extend(errors.iter().cloned());
        }
        let mut errors = Vec::new();
        self.walk(target, value, path, &mut errors);
        out.extend(errors.iter().cloned());
        self.memo.insert(key, (path.to_string(), errors));
    }

    fn walk_object(
        &mut self,
        schema: &'a Map<String, Value>,
        object: &Map<String, Value>,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        let properties = schema.get("properties").and_then(Value::as_object).unwrap_or(&EMPTY);
        for key in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                out.push(error(&pointer(path, key), "required", "missing".into()));
            }
        }
        for (key, property) in properties {
            if let Some(item) = object.get(key) {
                self.walk(property, item, &pointer(path, key), out);
            }
        }
        let patterns: Vec<(regex::Regex, &Value)> = match schema.get("patternProperties") {
            Some(Value::Object(patterns)) => {
                patterns.iter().filter_map(|(pattern, item)| Some((regex::Regex::new(pattern).ok()?, item))).collect()
            }
            _ => Vec::new(),
        };
        for (key, item) in object {
            let mut matched = properties.contains_key(key);
            for (pattern, item_schema) in &patterns {
                if pattern.is_match(key) {
                    matched = true;
                    self.walk(item_schema, item, &pointer(path, key), out);
                }
            }
            match schema.get("additionalProperties") {
                Some(Value::Bool(false)) if !matched => {
                    out.push(error(&pointer(path, key), "additionalProperties", "unknown field".into()));
                }
                Some(additional) if !matched => self.walk(additional, item, &pointer(path, key), out),
                _ => {}
            }
            if let Some(names) = schema.get("propertyNames")
                && !self.ok(names, &Value::String(key.clone()))
            {
                out.push(error(&pointer(path, key), "propertyNames", "invalid property name".into()));
            }
        }
        let count = object.len() as f64;
        if let Some(limit) = schema.get("minProperties").and_then(Value::as_f64).filter(|limit| count < *limit) {
            out.push(error(path, "minProperties", format!("expected at least {} properties", js_number(limit))));
        }
        if let Some(limit) = schema.get("maxProperties").and_then(Value::as_f64).filter(|limit| count > *limit) {
            out.push(error(path, "maxProperties", format!("expected at most {} properties", js_number(limit))));
        }
        let mut dependencies: Vec<(&String, &'a Value)> = Vec::new();
        for keyword in ["dependentRequired", "dependencies"] {
            for (key, dependency) in schema.get(keyword).and_then(Value::as_object).into_iter().flatten() {
                dependencies.retain(|(existing, _)| *existing != key);
                dependencies.push((key, dependency));
            }
        }
        for (key, dependency) in dependencies.into_iter().filter(|(key, _)| object.contains_key(*key)) {
            let Value::Array(names) = dependency else {
                self.walk(dependency, value, path, out);
                continue;
            };
            for name in names.iter().filter_map(Value::as_str).filter(|name| !object.contains_key(*name)) {
                out.push(error(&pointer(path, name), "dependentRequired", format!("required when {key} is present")));
            }
        }
        for (key, dependent) in schema.get("dependentSchemas").and_then(Value::as_object).into_iter().flatten() {
            if object.contains_key(key) {
                self.walk(dependent, value, path, out);
            }
        }
    }

    fn walk_array(
        &mut self,
        schema: &'a Map<String, Value>,
        items: &[Value],
        path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        let (prefix, rest): (&'a [Value], Option<&'a Value>) = match schema.get("items") {
            Some(Value::Array(tuple)) => (tuple, schema.get("additionalItems")),
            items => (schema.get("prefixItems").and_then(Value::as_array).map_or(&[], Vec::as_slice), items),
        };
        for (index, item) in items.iter().enumerate() {
            if let Some(item_schema) = prefix.get(index).or(rest) {
                self.walk(item_schema, item, &format!("{path}/{index}"), out);
            }
        }
        let count = items.len() as f64;
        if let Some(limit) = schema.get("minItems").and_then(Value::as_f64).filter(|limit| count < *limit) {
            out.push(error(path, "minItems", format!("expected at least {} items", js_number(limit))));
        }
        if let Some(limit) = schema.get("maxItems").and_then(Value::as_f64).filter(|limit| count > *limit) {
            out.push(error(path, "maxItems", format!("expected at most {} items", js_number(limit))));
        }
        let unique = |index: usize| items[..index].iter().all(|earlier| !json_equal(earlier, &items[index]));
        if schema.get("uniqueItems") == Some(&Value::Bool(true)) && !(1..items.len()).all(unique) {
            out.push(error(path, "uniqueItems", "expected unique items".into()));
        }
        if let Some(contains) = schema.get("contains") {
            let min = schema.get("minContains").and_then(Value::as_f64).unwrap_or(1.0);
            let max = schema.get("maxContains").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
            let matched = items.iter().filter(|item| self.ok(contains, item)).count() as f64;
            if matched < min || matched > max {
                out.push(error(path, "contains", contains_message(min, max)));
            }
        }
    }

    fn walk_union(
        &mut self,
        branches: &'a [Value],
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationError>,
        exactly_one: bool,
    ) {
        let matches = branches.iter().filter(|branch| self.ok(branch, value)).count();
        if matches == 1 || (!exactly_one && matches > 0) {
            return;
        }
        let keyword = if exactly_one { "oneOf" } else { "anyOf" };
        if matches > 1 {
            return out.push(error(path, keyword, "matches more than one alternative".into()));
        }
        // A tagged union reports the branch its tag selects.
        if let Some((key, values)) = self.tag_of(branches)
            && let Value::Object(object) = value
        {
            let selected = object.get(key).and_then(|tag| values.iter().position(|value| strict_equal(value, tag)));
            match selected {
                Some(index) => self.walk(&branches[index], value, path, out),
                None => {
                    let listed: Vec<Value> = values.into_iter().cloned().collect();
                    out.push(error(&pointer(path, key), keyword, format!("expected one of {}", list_values(&listed))));
                }
            }
            return;
        }
        // An object is blamed on the branch that declares most of its properties, then on the branch
        // whose first error is deepest.
        let mut best: Option<(Vec<ValidationError>, usize)> = None;
        for branch in branches {
            let mut errors = Vec::new();
            self.walk(branch, value, path, &mut errors);
            if errors.is_empty() {
                continue;
            }
            let declared = value.as_object().map_or(0, |object| self.declared_keys(branch, object));
            let depth = |errors: &[ValidationError]| errors[0].path.encode_utf16().count();
            let better = best.as_ref().is_none_or(|(best_errors, best_declared)| {
                declared > *best_declared || (declared == *best_declared && depth(&errors) > depth(best_errors))
            });
            if better {
                best = Some((errors, declared));
            }
        }
        match best {
            Some((errors, _)) => out.extend(errors),
            None => out.push(error(path, keyword, "matches no alternative".into())),
        }
    }

    /// A branch, or the schema its `$ref` names.
    fn target(&self, branch: &'a Value) -> Option<&'a Value> {
        match branch.get("$ref").and_then(Value::as_str) {
            Some(reference) => self.resolve(reference),
            None => Some(branch),
        }
    }

    /// How many keys of `object` the branch declares in `properties`.
    fn declared_keys(&self, branch: &'a Value, object: &Map<String, Value>) -> usize {
        let properties = self.target(branch).and_then(|node| node.get("properties")).and_then(Value::as_object);
        properties.map_or(0, |properties| object.keys().filter(|key| properties.contains_key(*key)).count())
    }

    /// The first property of the first branch whose `const` is a distinct primitive in every branch.
    fn tag_of(&self, branches: &'a [Value]) -> Option<(&'a str, Vec<&'a Value>)> {
        let resolved: Vec<Option<&'a Value>> = branches.iter().map(|branch| self.target(branch)).collect();
        let first = resolved.first().copied().flatten()?.get("properties")?.as_object()?;
        first.keys().find_map(|key| {
            let values: Vec<&'a Value> = resolved
                .iter()
                .map(|node| node.and_then(|node| node.get("properties")?.get(key.as_str())?.get("const")))
                .collect::<Option<_>>()?;
            let primitive = values.iter().all(|value| !value.is_object() && !value.is_array());
            let distinct = values
                .iter()
                .enumerate()
                .all(|(index, value)| values[..index].iter().all(|earlier| !strict_equal(earlier, value)));
            (primitive && distinct).then_some((key.as_str(), values))
        })
    }
}

fn error(path: &str, keyword: &'static str, message: String) -> ValidationError {
    ValidationError { path: path.to_string(), keyword, message }
}

fn walk_string(schema: &Map<String, Value>, text: &str, path: &str, out: &mut Vec<ValidationError>) {
    let length = text.chars().count() as f64;
    if let Some(limit) = schema.get("minLength").and_then(Value::as_f64).filter(|limit| length < *limit) {
        out.push(error(path, "minLength", format!("expected at least {} characters", js_number(limit))));
    }
    if let Some(limit) = schema.get("maxLength").and_then(Value::as_f64).filter(|limit| length > *limit) {
        out.push(error(path, "maxLength", format!("expected at most {} characters", js_number(limit))));
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
        && regex::Regex::new(pattern).is_ok_and(|regex| !regex.is_match(text))
    {
        out.push(error(path, "pattern", format!("expected to match {pattern}")));
    }
    if let Some(format) = schema.get("format").and_then(Value::as_str)
        && !test_format(format, text)
    {
        out.push(error(path, "format", format!("expected format {format}")));
    }
}

fn walk_number(schema: &Map<String, Value>, number: f64, path: &str, out: &mut Vec<ValidationError>) {
    let bound = |keyword: &str| schema.get(keyword).and_then(Value::as_f64);
    let checks: [BoundCheck; 4] = [
        ("minimum", ">=", |value, limit| value >= limit),
        ("maximum", "<=", |value, limit| value <= limit),
        ("exclusiveMinimum", ">", |value, limit| value > limit),
        ("exclusiveMaximum", "<", |value, limit| value < limit),
    ];
    for (keyword, comparison, passes) in checks {
        if let Some(limit) = bound(keyword).filter(|limit| !passes(number, *limit)) {
            out.push(error(path, keyword, format!("expected {comparison} {}", js_number(limit))));
        }
    }
    if let Some(divisor) = bound("multipleOf").filter(|divisor| !is_multiple(number, *divisor)) {
        out.push(error(path, "multipleOf", format!("expected a multiple of {}", js_number(divisor))));
    }
}

/// Whether `value` is a multiple of `divisor`, with the tolerance of the check.
fn is_multiple(value: f64, divisor: f64) -> bool {
    let quotient = value / divisor;
    divisor == 0.0 || (quotient - quotient.round()).abs() <= 1e-10
}

fn contains_message(min: f64, max: f64) -> String {
    let noun = |count: f64| if count == 1.0 { "item" } else { "items" };
    if max == f64::INFINITY {
        return format!("expected at least {} matching {}", js_number(min), noun(min));
    }
    if min == 0.0 {
        return format!("expected at most {} matching {}", js_number(max), noun(max));
    }
    format!("expected between {} and {} matching items", js_number(min), js_number(max))
}

/// `values` as JSON, at most [`LISTED_VALUES`] of them, then how many more there are.
fn list_values(values: &[Value]) -> String {
    if values.len() <= LISTED_VALUES {
        return js_json(&Value::Array(values.to_vec()));
    }
    let listed = js_json(&Value::Array(values[..LISTED_VALUES].to_vec()));
    format!("{}, ... and {} more]", &listed[..listed.len() - 1], values.len() - LISTED_VALUES)
}

/// `JSON.stringify(value)`: numbers as JavaScript prints them.
pub(crate) fn js_json(value: &Value) -> String {
    match value {
        Value::Number(number) if number.is_f64() => js_number(number.as_f64().unwrap_or_default()),
        Value::Array(items) => format!("[{}]", items.iter().map(js_json).collect::<Vec<_>>().join(",")),
        Value::Object(object) => {
            let members: Vec<String> =
                object.iter().map(|(key, item)| format!("{}:{}", Value::String(key.clone()), js_json(item))).collect();
            format!("{{{}}}", members.join(","))
        }
        other => other.to_string(),
    }
}

/// `String(value)` of a `type` entry.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => js_json(other),
    }
}

/// JavaScript `===` on JSON values: primitives by value, objects and arrays never equal.
pub(crate) fn strict_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Object(_) | Value::Array(_), _) | (_, Value::Object(_) | Value::Array(_)) => false,
        _ => left == right,
    }
}

fn pointer(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::Number(number) if number.as_f64().is_some_and(|number| number.fract() == 0.0) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "object",
    }
}
