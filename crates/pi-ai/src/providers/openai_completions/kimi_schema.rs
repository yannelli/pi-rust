use serde_json::{Map, Value};

/// Moonshot rejects enum-only MCP properties even though they are valid JSON Schema.
pub(super) fn normalize(schema: &mut Value) {
    let Some(node) = schema.as_object_mut() else { return };
    let composed = ["$ref", "anyOf", "oneOf", "allOf", "not", "if", "then", "else"];
    if !node.contains_key("type") && !composed.iter().any(|key| node.contains_key(*key)) {
        if let Some(kind) = infer_type(node) {
            node.insert("type".into(), Value::String(kind.into()));
        }
    }
    for key in ["properties", "patternProperties", "$defs", "definitions", "dependentSchemas", "dependencies"] {
        if let Some(children) = node.get_mut(key).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                normalize(child);
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems", "items"] {
        if let Some(children) = node.get_mut(key).and_then(Value::as_array_mut) {
            for child in children {
                normalize(child);
            }
        }
    }
    for key in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
        "propertyNames",
        "contentSchema",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(child) = node.get_mut(key) {
            normalize(child);
        }
    }
}

fn infer_type(node: &Map<String, Value>) -> Option<&'static str> {
    if let Some(values) = node.get("enum").and_then(Value::as_array).filter(|values| !values.is_empty()) {
        let mut kind = value_type(&values[0]);
        for value in &values[1..] {
            let next = value_type(value);
            if matches!((kind, next), ("integer", "number") | ("number", "integer")) {
                kind = "number";
            } else if kind != next {
                return None;
            }
        }
        return Some(kind);
    }
    if let Some(value) = node.get("const") {
        return Some(value_type(value));
    }
    let structure: &[(&str, &[&str])] = &[
        (
            "object",
            &[
                "properties",
                "patternProperties",
                "additionalProperties",
                "required",
                "dependentSchemas",
                "dependencies",
                "dependentRequired",
                "propertyNames",
                "minProperties",
                "maxProperties",
                "unevaluatedProperties",
            ],
        ),
        (
            "array",
            &[
                "items",
                "prefixItems",
                "contains",
                "additionalItems",
                "minItems",
                "maxItems",
                "uniqueItems",
                "minContains",
                "maxContains",
                "unevaluatedItems",
            ],
        ),
        (
            "string",
            &["format", "pattern", "minLength", "maxLength", "contentEncoding", "contentMediaType", "contentSchema"],
        ),
        ("number", &["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf"]),
    ];
    let mut matches = structure.iter().filter(|(_, keys)| keys.iter().any(|key| node.contains_key(*key)));
    let (kind, _) = matches.next()?;
    matches.next().is_none().then_some(*kind)
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.as_f64().is_some_and(|number| number.fract() == 0.0) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
