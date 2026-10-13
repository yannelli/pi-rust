//! JSON Schema checks with TypeBox's keyword order and error messages (`en_US` locale).

use std::collections::HashSet;

use regex::Regex;
use serde_json::{Map, Value};

use super::formats::test_format;

/// TypeBox stops collecting errors at this many per context.
const MAX_ERRORS: usize = 8;

/// One failed keyword: where, and the message TypeBox prints for it.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaError {
    pub keyword: &'static str,
    pub instance_path: String,
    pub message: String,
    /// For `required`: the missing properties.
    pub required: Vec<String>,
}

/// A numeric bound keyword, its comparison as TypeBox prints it, and the passing test.
type BoundCheck = (&'static str, &'static str, fn(f64, f64) -> bool);

pub struct Checker<'a> {
    root: &'a Value,
    pub errors: Vec<SchemaError>,
}

/// Numbers as JavaScript prints them: integers without a fraction.
pub fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 { format!("{}", value as i64) } else { value.to_string() }
}

fn schema_types(schema: &Map<String, Value>) -> Option<Vec<&str>> {
    match schema.get("type")? {
        Value::String(kind) => Some(vec![kind.as_str()]),
        Value::Array(kinds) => Some(kinds.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

pub fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// Deep equality where `1` equals `1.0`, as in JavaScript.
pub fn json_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| json_equal(a, b)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, value)| b.get(key).is_some_and(|other| json_equal(value, other)))
        }
        _ => left == right,
    }
}

fn regex(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
}

impl<'a> Checker<'a> {
    pub fn new(root: &'a Value) -> Self {
        Self { root, errors: Vec::new() }
    }

    /// Whether `value` matches `schema`, without keeping errors.
    pub fn is_valid(&self, value: &Value, schema: &Value) -> bool {
        Checker::new(self.root).check(value, schema, "")
    }

    fn add(&mut self, keyword: &'static str, path: &str, message: String) -> bool {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(SchemaError { keyword, instance_path: path.into(), message, required: Vec::new() });
        }
        false
    }

    /// Resolve a local `$ref` (`#`, `#/$defs/x`, `#/definitions/x`) against the root schema.
    fn resolve(&self, reference: &str) -> Option<&'a Value> {
        let pointer = reference.strip_prefix('#')?;
        if pointer.is_empty() {
            return Some(self.root);
        }
        let mut current = self.root;
        for token in pointer.trim_start_matches('/').split('/') {
            let token = token.replace("~1", "/").replace("~0", "~");
            current = match current {
                Value::Object(object) => object.get(&token)?,
                Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// Check `value` against `schema`, collecting errors; every keyword runs, like TypeBox.
    pub fn check(&mut self, value: &Value, schema: &Value, path: &str) -> bool {
        if self.errors.len() >= MAX_ERRORS {
            return false;
        }
        let schema = match schema {
            Value::Bool(true) => return true,
            Value::Bool(false) => return self.add("boolean", path, "schema is false".into()),
            Value::Object(schema) => schema,
            _ => return true,
        };
        let mut ok = true;
        if let Some(kinds) = schema_types(schema)
            && !kinds.iter().any(|kind| matches_type(value, kind))
        {
            let message = match schema.get("type") {
                Some(Value::String(kind)) => format!("must be {kind}"),
                _ => format!("must be either {}", kinds.join(" or ")),
            };
            ok &= self.add("type", path, message);
        }
        match value {
            Value::Object(object) => ok &= self.check_object(object, schema, path),
            Value::Array(items) => ok &= self.check_array(items, schema, path),
            Value::String(text) => ok &= self.check_string(text, schema, path),
            Value::Number(number) => ok &= self.check_number(number.as_f64().unwrap_or_default(), schema, path),
            _ => {}
        }
        if let Some(target) = schema.get("$ref").and_then(Value::as_str)
            && let Some(resolved) = self.resolve(target)
        {
            ok &= self.check(value, resolved, path);
        }
        if let Some(expected) = schema.get("const")
            && !json_equal(expected, value)
        {
            ok &= self.add("const", path, "must be equal to constant".into());
        }
        if let Some(Value::Array(options)) = schema.get("enum")
            && !options.iter().any(|option| json_equal(option, value))
        {
            ok &= self.add("enum", path, "must be equal to one of the allowed values".into());
        }
        if let Some(condition) = schema.get("if") {
            ok &= self.check_if(value, condition, schema, path);
        }
        if let Some(negated) = schema.get("not")
            && self.is_valid(value, negated)
        {
            ok &= self.add("not", path, "must not be valid".into());
        }
        if let Some(Value::Array(schemas)) = schema.get("allOf") {
            ok &= self.check_all_of(value, schemas, path);
        }
        if let Some(Value::Array(schemas)) = schema.get("anyOf") {
            ok &= self.check_any_of(value, schemas, path);
        }
        if let Some(Value::Array(schemas)) = schema.get("oneOf") {
            ok &= self.check_one_of(value, schemas, path);
        }
        ok
    }

    /// Errors of `schema` alone, in a fresh context (TypeBox runs logical operands this way).
    fn errors_of(&self, value: &Value, schema: &Value, path: &str) -> Option<Vec<SchemaError>> {
        let mut nested = Checker::new(self.root);
        (!nested.check(value, schema, path)).then_some(nested.errors)
    }

    fn merge(&mut self, errors: Vec<SchemaError>) {
        for error in errors {
            if self.errors.len() < MAX_ERRORS {
                self.errors.push(error);
            }
        }
    }

    fn check_if(&mut self, value: &Value, condition: &Value, schema: &Map<String, Value>, path: &str) -> bool {
        let (branch, name) =
            if self.is_valid(value, condition) { (schema.get("then"), "then") } else { (schema.get("else"), "else") };
        match branch {
            Some(branch) if !self.is_valid(value, branch) => {
                self.add("if", path, format!("must match \"{name}\" schema"))
            }
            _ => true,
        }
    }

    fn check_all_of(&mut self, value: &Value, schemas: &[Value], path: &str) -> bool {
        let failed: Vec<Vec<SchemaError>> =
            schemas.iter().filter_map(|schema| self.errors_of(value, schema, path)).collect();
        let ok = failed.is_empty();
        failed.into_iter().for_each(|errors| self.merge(errors));
        ok
    }

    fn check_any_of(&mut self, value: &Value, schemas: &[Value], path: &str) -> bool {
        let results: Vec<Option<Vec<SchemaError>>> =
            schemas.iter().map(|schema| self.errors_of(value, schema, path)).collect();
        if results.iter().any(Option::is_none) {
            return true;
        }
        results.into_iter().flatten().for_each(|errors| self.merge(errors));
        self.add("anyOf", path, "must match a schema in anyOf".into())
    }

    fn check_one_of(&mut self, value: &Value, schemas: &[Value], path: &str) -> bool {
        let results: Vec<Option<Vec<SchemaError>>> =
            schemas.iter().map(|schema| self.errors_of(value, schema, path)).collect();
        let passing = results.iter().filter(|result| result.is_none()).count();
        if passing == 1 {
            return true;
        }
        if passing == 0 {
            results.into_iter().flatten().for_each(|errors| self.merge(errors));
        }
        self.add("oneOf", path, "must match exactly one schema in oneOf".into())
    }

    fn property_patterns(schema: &Map<String, Value>) -> Vec<(Regex, &Value)> {
        let Some(Value::Object(patterns)) = schema.get("patternProperties") else { return Vec::new() };
        patterns.iter().filter_map(|(pattern, schema)| Some((regex(pattern)?, schema))).collect()
    }

    fn check_object(&mut self, object: &Map<String, Value>, schema: &Map<String, Value>, path: &str) -> bool {
        let mut ok = true;
        if let Some(Value::Array(required)) = schema.get("required") {
            let missing: Vec<String> = required
                .iter()
                .filter_map(Value::as_str)
                .filter(|key| !object.contains_key(*key))
                .map(String::from)
                .collect();
            if !missing.is_empty() {
                let message = format!("must have required properties {}", missing.join(", "));
                ok &= self.add("required", path, message);
                if let Some(error) = self.errors.last_mut().filter(|error| error.keyword == "required") {
                    error.required = missing;
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        let patterns = Self::property_patterns(schema);
        if let Some(additional) = schema.get("additionalProperties") {
            let mut extra = false;
            for (key, item) in object {
                let known = properties.is_some_and(|properties| properties.contains_key(key))
                    || patterns.iter().any(|(pattern, _)| pattern.is_match(key));
                if !known && !self.check(item, additional, &format!("{path}/{key}")) {
                    extra = true;
                }
            }
            if extra {
                ok &= self.add("additionalProperties", path, "must not have additional properties".into());
            }
        }
        if let Some(Value::Object(dependent)) = schema.get("dependentRequired") {
            for (property, dependencies) in dependent.iter().filter(|(property, _)| object.contains_key(*property)) {
                let names: Vec<&str> =
                    dependencies.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if names.iter().any(|name| !object.contains_key(*name)) {
                    let message =
                        format!("must have properties {} when property {property} is present", names.join(", "));
                    ok &= self.add("dependentRequired", path, message);
                }
            }
        }
        for (pattern, property_schema) in &patterns {
            for (key, item) in object.iter().filter(|(key, _)| pattern.is_match(key)) {
                ok &= self.check(item, property_schema, &format!("{path}/{key}"));
            }
        }
        for (key, property_schema) in properties.into_iter().flatten() {
            if let Some(item) = object.get(key) {
                ok &= self.check(item, property_schema, &format!("{path}/{key}"));
            }
        }
        if let Some(names) = schema.get("propertyNames") {
            let invalid: Vec<&str> = object
                .keys()
                .filter(|key| !self.is_valid(&Value::String((*key).clone()), names))
                .map(String::as_str)
                .collect();
            if !invalid.is_empty() {
                ok &= self.add("propertyNames", path, format!("property names {} are invalid", invalid.join(", ")));
            }
        }
        let count = object.len() as f64;
        if let Some(limit) = schema.get("minProperties").and_then(Value::as_f64).filter(|limit| count < *limit) {
            ok &= self.add("minProperties", path, format!("must not have fewer than {} properties", js_number(limit)));
        }
        if let Some(limit) = schema.get("maxProperties").and_then(Value::as_f64).filter(|limit| count > *limit) {
            ok &= self.add("maxProperties", path, format!("must not have more than {} properties", js_number(limit)));
        }
        ok
    }

    fn check_array(&mut self, items: &[Value], schema: &Map<String, Value>, path: &str) -> bool {
        let mut ok = true;
        if let Some(contains) = schema.get("contains") {
            let matched = items.iter().filter(|item| self.is_valid(item, contains)).count() as f64;
            let min = schema.get("minContains").and_then(Value::as_f64).unwrap_or(1.0);
            if matched < min {
                ok &= self.add("contains", path, "must contain at least 1 valid item".into());
            }
            if let Some(max) = schema.get("maxContains").and_then(Value::as_f64).filter(|max| matched > *max) {
                ok &= self.add(
                    "maxContains",
                    path,
                    format!("must not contain more than {} matching items", js_number(max)),
                );
            }
        }
        let prefix = match schema.get("prefixItems") {
            Some(Value::Array(prefix)) => prefix.as_slice(),
            _ => &[],
        };
        match schema.get("items") {
            Some(Value::Array(tuple)) => {
                for (index, (item, item_schema)) in items.iter().zip(tuple).enumerate() {
                    ok &= self.check(item, item_schema, &format!("{path}/{index}"));
                }
                if let Some(additional) = schema.get("additionalItems") {
                    for (index, item) in items.iter().enumerate().skip(tuple.len()) {
                        ok &= self.check(item, additional, &format!("{path}/{index}"));
                    }
                }
            }
            Some(item_schema) => {
                for (index, item) in items.iter().enumerate().skip(prefix.len()) {
                    ok &= self.check(item, item_schema, &format!("{path}/{index}"));
                }
            }
            None => {}
        }
        let count = items.len() as f64;
        if let Some(limit) = schema.get("maxItems").and_then(Value::as_f64).filter(|limit| count > *limit) {
            ok &= self.add("maxItems", path, format!("must not have more than {} items", js_number(limit)));
        }
        if let Some(limit) = schema.get("minItems").and_then(Value::as_f64).filter(|limit| count < *limit) {
            ok &= self.add("minItems", path, format!("must not have fewer than {} items", js_number(limit)));
        }
        for (index, (item, item_schema)) in items.iter().zip(prefix).enumerate() {
            ok &= self.check(item, item_schema, &format!("{path}/{index}"));
        }
        if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
            let distinct: HashSet<String> = items.iter().map(Value::to_string).collect();
            if distinct.len() != items.len() {
                ok &= self.add("uniqueItems", path, "must not have duplicate items".into());
            }
        }
        ok
    }

    fn check_string(&mut self, text: &str, schema: &Map<String, Value>, path: &str) -> bool {
        let mut ok = true;
        let length = text.chars().count() as f64;
        if let Some(limit) = schema.get("maxLength").and_then(Value::as_f64).filter(|limit| length > *limit) {
            ok &= self.add("maxLength", path, format!("must not have more than {} characters", js_number(limit)));
        }
        if let Some(limit) = schema.get("minLength").and_then(Value::as_f64).filter(|limit| length < *limit) {
            ok &= self.add("minLength", path, format!("must not have fewer than {} characters", js_number(limit)));
        }
        if let Some(format) = schema.get("format").and_then(Value::as_str)
            && !test_format(format, text)
        {
            ok &= self.add("format", path, format!("must match format \"{format}\""));
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
            && regex(pattern).is_some_and(|regex| !regex.is_match(text))
        {
            ok &= self.add("pattern", path, format!("must match pattern \"{pattern}\""));
        }
        ok
    }

    fn check_number(&mut self, number: f64, schema: &Map<String, Value>, path: &str) -> bool {
        let mut ok = true;
        let bound = |keyword: &str| schema.get(keyword).and_then(Value::as_f64);
        let checks: [BoundCheck; 4] = [
            ("exclusiveMaximum", "<", |value, limit| value < limit),
            ("exclusiveMinimum", ">", |value, limit| value > limit),
            ("maximum", "<=", |value, limit| value <= limit),
            ("minimum", ">=", |value, limit| value >= limit),
        ];
        for (keyword, comparison, passes) in checks {
            if let Some(limit) = bound(keyword).filter(|limit| !passes(number, *limit)) {
                ok &= self.add(keyword, path, format!("must be {comparison} {}", js_number(limit)));
            }
        }
        if let Some(divisor) = bound("multipleOf").filter(|divisor| *divisor != 0.0) {
            let quotient = number / divisor;
            if (quotient - quotient.round()).abs() > 1e-10 {
                ok &= self.add("multipleOf", path, format!("must be multiple of {}", js_number(divisor)));
            }
        }
        ok
    }
}
