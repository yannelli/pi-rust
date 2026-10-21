//! TypeBox `Value.Default`: fills missing values from `default` annotations.

use serde_json::Value;

use crate::check::Checker;
use crate::kind::{Kind, evaluate_intersect, kind, record_entry};

/// TypeBox `Value.Default(schema, value)`. `None` is JavaScript `undefined`: a missing value that a
/// `default` annotation may supply. The result is unchecked; validate it before use.
pub fn default_value(schema: &Value, value: Option<Value>) -> Option<Value> {
    Defaults { root: schema }.visit_type(schema, value)
}

struct Defaults<'a> {
    root: &'a Value,
}

impl Defaults<'_> {
    fn visit_type(&self, schema: &Value, value: Option<Value>) -> Option<Value> {
        let defaulted = match (value, schema.get("default")) {
            (None, Some(default)) => Some(default.clone()),
            (value, _) => value,
        };
        let value = defaulted?;
        Some(match kind(schema) {
            Kind::Array => self.visit_array(schema, value),
            Kind::Intersect => self.visit_intersect(schema, value),
            Kind::Object => self.visit_object(schema, value),
            Kind::Record => self.visit_record(schema, value),
            Kind::Tuple => self.visit_tuple(schema, value),
            Kind::Union => self.visit_union(schema, value),
            Kind::Other => value,
        })
    }

    fn visit_array(&self, schema: &Value, mut value: Value) -> Value {
        let (Some(items), Value::Array(values)) = (schema.get("items"), &mut value) else { return value };
        values.iter_mut().for_each(|slot| self.replace(items, slot));
        value
    }

    fn visit_intersect(&self, schema: &Value, value: Value) -> Value {
        if let Some(evaluated) = evaluate_intersect(schema) {
            return self.visit_object(&evaluated, value);
        }
        let branches = schema.get("allOf").and_then(Value::as_array).cloned().unwrap_or_default();
        branches.iter().fold(value, |value, branch| self.visit_type(branch, Some(value.clone())).unwrap_or(value))
    }

    fn visit_object(&self, schema: &Value, mut value: Value) -> Value {
        let Value::Object(object) = &mut value else { return value };
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, property) in properties.into_iter().flatten() {
            match object.get_mut(key) {
                Some(slot) => self.replace(property, slot),
                None => {
                    if let Some(next) = self.visit_type(property, None) {
                        object.insert(key.clone(), next);
                    }
                }
            }
        }
        let Some(additional @ Value::Object(_)) = schema.get("additionalProperties") else { return value };
        for (_, slot) in object.iter_mut().filter(|(key, _)| !properties.is_some_and(|known| known.contains_key(*key)))
        {
            self.replace(additional, slot);
        }
        value
    }

    /// Defaults applied to a present value, in place.
    fn replace(&self, schema: &Value, slot: &mut Value) {
        if let Some(next) = self.visit_type(schema, Some(slot.take())) {
            *slot = next;
        }
    }

    fn visit_record(&self, schema: &Value, mut value: Value) -> Value {
        let (Some((pattern, item)), Value::Object(object)) = (record_entry(schema), &mut value) else {
            return value;
        };
        let Ok(pattern) = regex::Regex::new(pattern) else { return value };
        if item.get("default").is_some() {
            for (_, slot) in object.iter_mut().filter(|(key, _)| pattern.is_match(key)) {
                self.replace(item, slot);
            }
        }
        if let Some(additional @ Value::Object(_)) = schema.get("additionalProperties") {
            for (_, slot) in object.iter_mut().filter(|(key, _)| !pattern.is_match(key)) {
                self.replace(additional, slot);
            }
        }
        value
    }

    fn visit_tuple(&self, schema: &Value, mut value: Value) -> Value {
        let (Some(Value::Array(items)), Value::Array(values)) = (schema.get("items"), &mut value) else {
            return value;
        };
        for (index, item) in items.iter().enumerate() {
            if let Some(slot) = values.get_mut(index) {
                self.replace(item, slot);
                continue;
            }
            // JavaScript would leave holes before a later default; JSON cannot hold them, so stop.
            match self.visit_type(item, None) {
                Some(next) if values.len() == index => values.push(next),
                _ => break,
            }
        }
        value
    }

    fn visit_union(&self, schema: &Value, value: Value) -> Value {
        for branch in schema.get("anyOf").and_then(Value::as_array).into_iter().flatten() {
            if let Some(result) = self.visit_type(branch, Some(value.clone()))
                && Checker::new(self.root).check(&result, branch, "")
            {
                return result;
            }
        }
        value
    }
}
