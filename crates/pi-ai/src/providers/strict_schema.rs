//! JSON-schema constrained sampling, ported from upstream `api/constrained-sampling.ts`.

use serde_json::{Value, json};

use crate::types::Tool;

const UNSUPPORTED_STRICT_KEYS: [&str; 16] = [
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn is_structured(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else { return false };
    let types: Vec<&str> = match &object.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    types.contains(&"object")
        || types.contains(&"array")
        || object.contains_key("properties")
        || object.contains_key("items")
}

fn allows_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else { return false };
    let null_type = match &object.get("type") {
        Some(Value::String(kind)) => kind == "null",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "null"),
        _ => false,
    };
    null_type
        || object.get("const") == Some(&Value::Null)
        || object.get("enum").and_then(Value::as_array).is_some_and(|values| values.contains(&Value::Null))
        || object.get("anyOf").and_then(Value::as_array).is_some_and(|variants| variants.iter().any(allows_null))
}

fn make_node_strict(schema: &mut Value) -> Result<(), String> {
    let Some(object) = schema.as_object_mut() else { return Err("boolean schemas are unsupported".into()) };
    if let Some(key) = UNSUPPORTED_STRICT_KEYS.iter().find(|key| object.contains_key(**key)) {
        return Err(format!("{key} schemas are unsupported"));
    }
    if let Some(any_of) = object.get_mut("anyOf") {
        let variants = any_of.as_array_mut().filter(|variants| !variants.is_empty());
        let variants = variants.ok_or("anyOf must contain at least one schema")?;
        for variant in variants {
            if is_structured(variant) {
                return Err("object and array unions are unsupported".into());
            }
            make_node_strict(variant)?;
        }
    }
    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".into());
        }
        make_node_strict(items)?;
    }
    let is_object = object.get("type").and_then(Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object {
        return Err("properties require type object".into());
    }
    if !is_object {
        return Ok(());
    }
    if object.get("additionalProperties").is_some_and(|value| *value != Value::Bool(false)) {
        return Err("schema-valued or true additionalProperties is unsupported".into());
    }
    if object.get("properties").is_some_and(|properties| !properties.is_object()) {
        return Err("object properties must be a schema map".into());
    }
    let required: Vec<String> = match object.get("required") {
        None => Vec::new(),
        Some(Value::Array(keys)) if keys.iter().all(Value::is_string) => {
            keys.iter().filter_map(Value::as_str).map(str::to_string).collect()
        }
        Some(_) => return Err("object required must be a string array".into()),
    };
    let mut empty = serde_json::Map::new();
    let properties = object.get_mut("properties").and_then(Value::as_object_mut).unwrap_or(&mut empty);
    if required.iter().any(|key| !properties.contains_key(key)) {
        return Err("required contains an unknown property".into());
    }
    for (key, property) in properties.iter_mut() {
        make_node_strict(property)?;
        if !required.contains(key) && !allows_null(property) {
            *property = json!({"anyOf": [property.take(), {"type": "null"}]});
        }
    }
    let names: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
    object.insert("required".into(), Value::Array(names));
    object.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

/// The strict subset of `schema` expected by provider constrained sampling.
pub(crate) fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return Err("root schema must have type object".into());
    }
    make_node_strict(&mut cloned)?;
    if cloned.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root schema must have type object".into());
    }
    Ok(cloned)
}

/// Whether `tool` is sent in strict mode: `Ok(Some(true))` when it opts in and its schema
/// converts, `Ok(None)` otherwise, and `Err` when it requires strict mode that cannot apply.
pub(crate) fn resolve_strict_sampling(tool: &Tool, supports_strict_mode: bool) -> Result<Option<bool>, String> {
    let Some(config) = tool.constrained_sampling.as_ref().filter(|config| config["type"] == "json_schema") else {
        return Ok(None);
    };
    let required = config["strict"] == "require";
    if supports_strict_mode {
        return match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(_) if !required => Ok(None),
            Err(error) => {
                Err(format!("Tool \"{}\" requires JSON-schema constrained sampling, but {error}.", tool.name))
            }
        };
    }
    if required {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(None)
}

/// Tool parameters, converted to the strict subset when `strict` is set.
pub(crate) fn tool_parameters(tool: &Tool, strict: Option<bool>) -> Value {
    match strict {
        Some(true) => make_strict_json_schema(&tool.parameters).unwrap_or_else(|_| tool.parameters.clone()),
        _ => tool.parameters.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_schema_requires_every_property_and_allows_null_for_optional_ones() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "number"}}, "required": ["a"]});
        let strict = make_strict_json_schema(&schema).unwrap();
        assert_eq!(strict["required"], json!(["a", "b"]));
        assert_eq!(strict["additionalProperties"], json!(false));
        assert_eq!(strict["properties"]["b"], json!({"anyOf": [{"type": "number"}, {"type": "null"}]}));
        assert!(make_strict_json_schema(&json!({"type": "object", "oneOf": []})).is_err());
    }

    #[test]
    fn required_strict_mode_fails_without_support() {
        let mut tool = Tool::new("t", "d", json!({"type": "object", "properties": {}}));
        tool.constrained_sampling = Some(json!({"type": "json_schema", "strict": "require"}));
        assert_eq!(resolve_strict_sampling(&tool, true), Ok(Some(true)));
        let error = resolve_strict_sampling(&tool, false).unwrap_err();
        assert!(error.starts_with("Tool \"t\" requires JSON-schema constrained sampling"));
        tool.constrained_sampling = Some(json!({"type": "json_schema", "strict": "prefer"}));
        assert_eq!(resolve_strict_sampling(&tool, false), Ok(None));
    }
}
