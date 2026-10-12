//! Tool argument coercion and validation for the JSON Schema subset tools use.
//! Coercion follows the TypeScript pi: models often send `"5"` for 5 or `null` for an omitted field.

use serde_json::{Map, Number, Value};

/// Coerce `arguments` toward `schema` and validate them. The error text matches
/// the TypeScript pi format so models see the same feedback.
pub fn validate_tool_arguments(tool_name: &str, schema: &Value, arguments: &Value) -> Result<Value, String> {
    let mut value = arguments.clone();
    drop_optional_nulls(&mut value, schema);
    value = coerce(value, schema);
    let mut errors = Vec::new();
    check(&value, schema, "", &mut errors);
    if errors.is_empty() {
        return Ok(value);
    }
    let errors: Vec<String> =
        errors.into_iter().map(|(path, message)| format!("  - {}: {message}", display_path(&path))).collect();
    Err(format!(
        "Validation failed for tool \"{tool_name}\":\n{}\n\nReceived arguments:\n{}",
        errors.join("\n"),
        serde_json::to_string_pretty(arguments).unwrap_or_default()
    ))
}

fn display_path(path: &str) -> &str {
    if path.is_empty() { "root" } else { path }
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn required(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn drop_optional_nulls(value: &mut Value, schema: &Value) {
    let (Value::Object(object), Some(Value::Object(properties))) = (value, schema.get("properties")) else { return };
    let required = required(schema);
    object.retain(|key, item| !(item.is_null() && !required.contains(&key.as_str()) && properties.contains_key(key)));
    for (key, item) in object.iter_mut() {
        if let Some(property) = properties.get(key) {
            drop_optional_nulls(item, property);
        }
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn coerce_primitive(value: &Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("number" | "integer", Value::Null) => Some(Value::from(0)),
        ("number" | "integer", Value::Bool(flag)) => Some(Value::from(u8::from(*flag))),
        ("number", Value::String(text)) if !text.trim().is_empty() => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .and_then(Number::from_f64)
            .map(Value::Number),
        ("integer", Value::String(text)) if !text.trim().is_empty() => {
            let number =
                text.trim().parse::<f64>().ok().filter(|number| number.is_finite() && number.fract() == 0.0)?;
            Some(Value::from(number as i64))
        }
        ("boolean", Value::Null) => Some(Value::Bool(false)),
        ("boolean", Value::String(text)) if text == "true" || text == "false" => Some(Value::Bool(text == "true")),
        ("boolean", Value::Number(number)) if number.as_f64() == Some(1.0) || number.as_f64() == Some(0.0) => {
            Some(Value::Bool(number.as_f64() == Some(1.0)))
        }
        ("string", Value::Null) => Some(Value::String(String::new())),
        ("string", Value::Number(number)) => Some(Value::String(number.to_string())),
        ("string", Value::Bool(flag)) => Some(Value::String(flag.to_string())),
        ("null", Value::String(text)) if text.is_empty() => Some(Value::Null),
        ("null", Value::Bool(false)) => Some(Value::Null),
        ("null", Value::Number(number)) if number.as_f64() == Some(0.0) => Some(Value::Null),
        // Models sometimes send an array or object as a JSON string.
        ("array" | "object", Value::String(text)) => {
            serde_json::from_str::<Value>(text).ok().filter(|parsed| matches_type(parsed, kind))
        }
        _ => None,
    }
}

fn coerce(mut value: Value, schema: &Value) -> Value {
    if let Some(Value::Array(schemas)) = schema.get("allOf") {
        for nested in schemas {
            value = coerce(value, nested);
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(Value::Array(schemas)) = schema.get(keyword)
            && !schemas.iter().any(|nested| is_valid(&value, nested))
        {
            if let Some(coerced) = schemas
                .iter()
                .map(|nested| coerce(value.clone(), nested))
                .find(|coerced| schemas.iter().any(|nested| is_valid(coerced, nested)))
            {
                value = coerced;
            }
        }
    }
    let kinds = schema_types(schema);
    if !kinds.is_empty()
        && !kinds.iter().any(|kind| matches_type(&value, kind))
        && let Some(coerced) = kinds.iter().find_map(|kind| coerce_primitive(&value, kind))
    {
        value = coerced;
    }
    match &mut value {
        Value::Object(object) => coerce_object(object, schema),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) {
                for item in items.iter_mut() {
                    *item = coerce(std::mem::take(item), item_schema);
                }
            }
        }
        _ => {}
    }
    value
}

fn coerce_object(object: &mut Map<String, Value>, schema: &Value) {
    let properties = schema.get("properties").and_then(Value::as_object);
    let additional = schema.get("additionalProperties").filter(|additional| additional.is_object());
    for (key, item) in object.iter_mut() {
        if let Some(property) = properties.and_then(|properties| properties.get(key)).or(additional) {
            *item = coerce(std::mem::take(item), property);
        }
    }
}

fn is_valid(value: &Value, schema: &Value) -> bool {
    let mut errors = Vec::new();
    check(value, schema, "", &mut errors);
    errors.is_empty()
}

fn check(value: &Value, schema: &Value, path: &str, errors: &mut Vec<(String, String)>) {
    let Some(schema_object) = schema.as_object() else { return };
    let kinds = schema_types(schema);
    if !kinds.is_empty() && !kinds.iter().any(|kind| matches_type(value, kind)) {
        errors.push((path.into(), format!("must be {}", kinds.join(" or "))));
        return;
    }
    if let Some(Value::Array(options)) = schema_object.get("enum")
        && !options.contains(value)
    {
        let options: Vec<String> = options.iter().map(Value::to_string).collect();
        errors.push((path.into(), format!("must be one of {}", options.join(", "))));
    }
    if let Some(expected) = schema_object.get("const")
        && expected != value
    {
        errors.push((path.into(), format!("must be {expected}")));
    }
    if let Some(Value::Array(schemas)) = schema_object.get("allOf") {
        for nested in schemas {
            check(value, nested, path, errors);
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(Value::Array(schemas)) = schema_object.get(keyword)
            && !schemas.iter().any(|nested| is_valid(value, nested))
        {
            errors.push((path.into(), "must match a schema in ".to_string() + keyword));
        }
    }
    check_bounds(value, schema_object, path, errors);
    match value {
        Value::Object(object) => check_object(object, schema, path, errors),
        Value::Array(items) => {
            if let Some(item_schema) = schema_object.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check(item, item_schema, &format!("{path}/{index}"), errors);
                }
            }
        }
        _ => {}
    }
}

fn check_bounds(value: &Value, schema: &Map<String, Value>, path: &str, errors: &mut Vec<(String, String)>) {
    let bound = |keyword: &str| schema.get(keyword).and_then(Value::as_f64);
    if let Some(number) = value.as_f64() {
        if let Some(minimum) = bound("minimum").filter(|minimum| number < *minimum) {
            errors.push((path.into(), format!("must be >= {minimum}")));
        }
        if let Some(maximum) = bound("maximum").filter(|maximum| number > *maximum) {
            errors.push((path.into(), format!("must be <= {maximum}")));
        }
    }
    let length = match value {
        Value::String(text) => Some(text.chars().count() as f64),
        Value::Array(items) => Some(items.len() as f64),
        _ => None,
    };
    if let Some(length) = length {
        let (min_key, max_key) = if value.is_string() { ("minLength", "maxLength") } else { ("minItems", "maxItems") };
        if let Some(minimum) = bound(min_key).filter(|minimum| length < *minimum) {
            errors.push((path.into(), format!("must have at least {minimum} items or characters")));
        }
        if let Some(maximum) = bound(max_key).filter(|maximum| length > *maximum) {
            errors.push((path.into(), format!("must have at most {maximum} items or characters")));
        }
    }
}

fn check_object(object: &Map<String, Value>, schema: &Value, path: &str, errors: &mut Vec<(String, String)>) {
    for name in required(schema) {
        if !object.contains_key(name) {
            errors.push((format!("{path}/{name}"), "is required".into()));
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    for (key, item) in object {
        let child = format!("{path}/{key}");
        match (properties.and_then(|properties| properties.get(key)), schema.get("additionalProperties")) {
            (Some(property), _) => check(item, property, &child, errors),
            (None, Some(Value::Bool(false))) => errors.push((child, "is not an allowed property".into())),
            (None, Some(additional)) if additional.is_object() => check(item, additional, &child, errors),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1},
                "verbose": {"type": "boolean"},
                "edits": {"type": "array", "items": {"type": "object", "properties": {"oldText": {"type": "string"}}, "required": ["oldText"]}},
                "mode": {"type": "string", "enum": ["a", "b"]}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn coerces_primitives_and_drops_optional_nulls() {
        let value = validate_tool_arguments(
            "t",
            &schema(),
            &json!({"path": 5, "limit": "10", "verbose": "true", "mode": null, "edits": "[{\"oldText\":\"x\"}]"}),
        )
        .unwrap();
        assert_eq!(value, json!({"path": "5", "limit": 10, "verbose": true, "edits": [{"oldText": "x"}]}));
    }

    #[test]
    fn reports_errors_in_upstream_format() {
        let error =
            validate_tool_arguments("read", &schema(), &json!({"limit": 0, "extra": 1, "mode": "c"})).unwrap_err();
        assert!(error.starts_with("Validation failed for tool \"read\":\n"), "{error}");
        assert!(error.contains("  - /path: is required"), "{error}");
        assert!(error.contains("  - /limit: must be >= 1"), "{error}");
        assert!(error.contains("  - /extra: is not an allowed property"), "{error}");
        assert!(error.contains("  - /mode: must be one of \"a\", \"b\""), "{error}");
        assert!(error.contains("Received arguments:"), "{error}");
    }
}
