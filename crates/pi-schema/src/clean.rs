//! TypeBox `Value.Clean`: removes properties and tuple elements the schema does not describe.

use serde_json::Value;

use crate::check::Checker;
use crate::kind::{Kind, evaluate_intersect, kind, record_entry, union_priority_sort};

/// TypeBox `Value.Clean(schema, value)`, with TypeBox's default union priority order: a union keeps
/// the first branch, narrowest first, whose cleaned value passes. The result is unchecked.
pub fn clean(schema: &Value, value: Value) -> Value {
    let sorted = union_priority_sort(schema);
    Cleaner { root: &sorted }.visit_type(&sorted, value)
}

struct Cleaner<'a> {
    root: &'a Value,
}

impl Cleaner<'_> {
    fn visit_type(&self, schema: &Value, value: Value) -> Value {
        match kind(schema) {
            Kind::Array => self.visit_array(schema, value),
            Kind::Intersect => match evaluate_intersect(schema) {
                Some(evaluated) => self.visit_object(&evaluated, value),
                None => value,
            },
            Kind::Object => self.visit_object(schema, value),
            Kind::Record => self.visit_record(schema, value),
            Kind::Tuple => self.visit_tuple(schema, value),
            Kind::Union => self.visit_union(schema, value),
            Kind::Other => value,
        }
    }

    fn replace(&self, schema: &Value, slot: &mut Value) {
        *slot = self.visit_type(schema, slot.take());
    }

    fn visit_array(&self, schema: &Value, mut value: Value) -> Value {
        if let (Some(items), Value::Array(values)) = (schema.get("items"), &mut value) {
            values.iter_mut().for_each(|slot| self.replace(items, slot));
        }
        value
    }

    /// What happens to a property the schema does not name: kept (and cleaned) or removed.
    fn keep_unknown(&self, schema: &Value, slot: &mut Value) -> bool {
        match schema.get("additionalProperties") {
            Some(Value::Bool(true)) => true,
            Some(additional @ Value::Object(_)) if Checker::new(self.root).check(slot, additional, "") => {
                self.replace(additional, slot);
                true
            }
            _ => false,
        }
    }

    fn visit_object(&self, schema: &Value, mut value: Value) -> Value {
        let Value::Object(object) = &mut value else { return value };
        let properties = schema.get("properties").and_then(Value::as_object);
        object.retain(|key, slot| match properties.and_then(|properties| properties.get(key)) {
            Some(property) => {
                self.replace(property, slot);
                true
            }
            None => self.keep_unknown(schema, slot),
        });
        value
    }

    fn visit_record(&self, schema: &Value, mut value: Value) -> Value {
        let (Some((pattern, item)), Value::Object(object)) = (record_entry(schema), &mut value) else {
            return value;
        };
        let Ok(pattern) = regex::Regex::new(pattern) else { return value };
        object.retain(|key, slot| {
            if pattern.is_match(key) {
                self.replace(item, slot);
                return true;
            }
            self.keep_unknown(schema, slot)
        });
        value
    }

    fn visit_tuple(&self, schema: &Value, mut value: Value) -> Value {
        if let (Some(Value::Array(items)), Value::Array(values)) = (schema.get("items"), &mut value) {
            values.truncate(items.len());
            values.iter_mut().zip(items).for_each(|(slot, item)| self.replace(item, slot));
        }
        value
    }

    fn visit_union(&self, schema: &Value, value: Value) -> Value {
        for branch in schema.get("anyOf").and_then(Value::as_array).into_iter().flatten() {
            let cleaned = self.visit_type(branch, value.clone());
            if Checker::new(self.root).check(&cleaned, branch, "") {
                return cleaned;
            }
        }
        value
    }
}
