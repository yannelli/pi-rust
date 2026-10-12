//! JSON Schema to TypeScript type expressions.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::identifier::is_identifier;
use crate::js::{js_stringify, js_trim, quote, utf16_cmp, utf16_len};

pub(crate) const INDENT: &str = "  ";
/// Local `$ref` expansions per rendered schema, so shared definitions cannot blow up the output.
const MAX_REF_EXPANSIONS: usize = 32;

/// A TypeScript type for a JSON Schema: sorted properties, `//` lines for described properties,
/// local `$ref`s resolved, and recursive or remote references as `unknown`. A result longer than
/// `max_chars` renders as `unknown`.
pub fn schema_to_type(schema: &Value, max_chars: Option<usize>) -> String {
    let mut context = SchemaContext { root: schema, resolving: HashSet::new(), expansions: 0 };
    let rendered = to_type(schema, &mut context);
    match max_chars {
        Some(limit) if utf16_len(&rendered) > limit => "unknown".into(),
        _ => rendered,
    }
}

struct SchemaContext<'a> {
    root: &'a Value,
    /// References being expanded on the current path, to stop at recursive types.
    resolving: HashSet<String>,
    expansions: usize,
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = segment.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn resolve_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    for segment in reference.get(2..).unwrap_or("").split('/').filter(|segment| !segment.is_empty()) {
        let key = percent_decode(segment)?.replace("~1", "/").replace("~0", "~");
        current = current.as_object()?.get(&key)?;
    }
    matches!(current, Value::Bool(_) | Value::Object(_)).then_some(current)
}

fn union(types: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for entry in types {
        if !unique.contains(&entry) {
            unique.push(entry);
        }
    }
    if unique.iter().any(|entry| entry == "unknown") {
        return "unknown".into();
    }
    if unique.is_empty() { "never".into() } else { unique.join(" | ") }
}

fn to_type(schema: &Value, context: &mut SchemaContext<'_>) -> String {
    let schema = match schema {
        Value::Bool(true) => return "unknown".into(),
        Value::Bool(false) => return "never".into(),
        Value::Object(schema) => schema,
        _ => return "unknown".into(),
    };
    if let Some(Value::String(reference)) = schema.get("$ref") {
        if context.resolving.contains(reference) || context.expansions >= MAX_REF_EXPANSIONS {
            return "unknown".into();
        }
        let Some(target) = resolve_ref(reference, context.root) else {
            return "unknown".into();
        };
        context.expansions += 1;
        context.resolving.insert(reference.clone());
        let rendered = to_type(target, context);
        context.resolving.remove(reference);
        return rendered;
    }

    if let Some(constant) = schema.get("const") {
        return js_stringify(constant);
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        return union(values.iter().map(js_stringify).collect());
    }

    let variants = match (schema.get("anyOf"), schema.get("oneOf")) {
        (Some(Value::Array(variants)), _) | (_, Some(Value::Array(variants))) => Some(variants),
        _ => None,
    };
    if let Some(variants) = variants {
        return union(variants.iter().map(|variant| to_type(variant, context)).collect());
    }
    if let Some(Value::Array(parts)) = schema.get("allOf") {
        let parts: Vec<String> =
            parts.iter().map(|part| to_type(part, context)).filter(|part| part != "unknown").collect();
        if parts.is_empty() {
            return "unknown".into();
        }
        let wrapped: Vec<String> =
            parts.into_iter().map(|part| if part.contains(" | ") { format!("({part})") } else { part }).collect();
        return wrapped.join(" & ");
    }

    match schema.get("type") {
        Some(Value::Array(types)) => union(
            types
                .iter()
                .map(|entry| {
                    let mut variant = schema.clone();
                    variant.insert("type".into(), entry.clone());
                    to_type(&Value::Object(variant), context)
                })
                .collect(),
        ),
        Some(Value::String(kind)) => match kind.as_str() {
            "string" => "string".into(),
            "number" | "integer" => "number".into(),
            "boolean" => "boolean".into(),
            "null" => "null".into(),
            "array" => array_type(schema, context),
            "object" => object_type(schema, context),
            _ => "unknown".into(),
        },
        Some(_) => "unknown".into(),
        None => {
            if ["properties", "additionalProperties", "required"].iter().any(|key| schema.contains_key(*key)) {
                object_type(schema, context)
            } else if schema.contains_key("items") || schema.contains_key("prefixItems") {
                array_type(schema, context)
            } else {
                "unknown".into()
            }
        }
    }
}

fn array_type(schema: &Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    match schema.get("items") {
        Some(Value::Array(_)) | None => {}
        Some(items) => return format!("Array<{}>", to_type(items, context)),
    }
    let tuple = match (schema.get("prefixItems"), schema.get("items")) {
        (Some(Value::Array(items)), _) | (_, Some(Value::Array(items))) => items.as_slice(),
        _ => &[],
    };
    if tuple.is_empty() {
        return "unknown[]".into();
    }
    let items: Vec<String> = tuple.iter().map(|item| to_type(item, context)).collect();
    format!("[{}]", items.join(", "))
}

fn description_of(property: &Value) -> &str {
    property.get("description").and_then(Value::as_str).map(js_trim).unwrap_or("")
}

fn property_key(name: &str) -> String {
    if is_identifier(name) { name.into() } else { quote(name) }
}

fn object_type(schema: &Map<String, Value>, context: &mut SchemaContext<'_>) -> String {
    let empty = Map::new();
    let properties = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    let required: Vec<&str> = match schema.get("required") {
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort_by(|a, b| utf16_cmp(a, b));
    let mut members: Vec<String> = names
        .iter()
        .map(|name| {
            let optional = if required.contains(&name.as_str()) { "" } else { "?" };
            format!("{}{optional}: {};", property_key(name), to_type(&properties[name.as_str()], context))
        })
        .collect();
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => members.push("[key: string]: unknown;".into()),
        Some(additional) => members.push(format!("[key: string]: {};", to_type(additional, context))),
        None if names.is_empty() => members.push("[key: string]: unknown;".into()),
        None => {}
    }
    if members.is_empty() {
        return "{}".into();
    }
    if !names.iter().any(|name| !description_of(&properties[name.as_str()]).is_empty()) {
        return format!("{{ {} }}", members.join(" "));
    }

    let mut lines = vec!["{".to_string()];
    for (index, name) in names.iter().enumerate() {
        for line in description_of(&properties[name.as_str()]).split('\n') {
            let line = js_trim(line.strip_suffix('\r').unwrap_or(line));
            if !line.is_empty() {
                lines.push(format!("{INDENT}// {line}"));
            }
        }
        lines.push(format!("{INDENT}{}", members[index].replace('\n', &format!("\n{INDENT}"))));
    }
    for member in &members[names.len()..] {
        lines.push(format!("{INDENT}{member}"));
    }
    lines.push("}".into());
    lines.join("\n")
}
