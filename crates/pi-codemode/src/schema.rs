//! JSON Schema to TypeScript type expressions.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::identifier::is_identifier;
use crate::js::{js_number, js_stringify, js_trim, quote, utf16_cmp, utf16_len};

pub(crate) const INDENT: &str = "  ";
/// Local `$ref` expansions per rendered schema, so shared definitions cannot blow up the output.
const MAX_REF_EXPANSIONS: usize = 32;
/// JSON Schema keywords with a number or string value shown as `@keyword value` property tags.
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];
/// Tag keywords whose values all hold at once, so only the largest (lower) or smallest (upper) matters.
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];
/// Longest property tag; longer ones (large defaults, long patterns) are left out.
const MAX_TAG_CHARS: usize = 80;

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}')
}

/// A TypeScript type for a JSON Schema: sorted properties, `//` lines for properties with a
/// description or constraints the type cannot express (`// @integer @minimum 1 @default 20`), local
/// `$ref`s resolved, and recursive or remote references as `unknown`. A result longer than
/// `max_chars` is rendered again without the constraint tags, and renders as `unknown` if it is still
/// too long.
pub fn schema_to_type(schema: &Value, max_chars: Option<usize>) -> String {
    let render = |tags: bool| {
        let mut context = SchemaContext { root: schema, resolving: HashSet::new(), expansions: 0, tags };
        to_type(schema, &mut context)
    };
    let rendered = render(true);
    let Some(limit) = max_chars else { return rendered };
    if utf16_len(&rendered) <= limit {
        return rendered;
    }
    let untagged = render(false);
    if utf16_len(&untagged) <= limit { untagged } else { "unknown".into() }
}

struct SchemaContext<'a> {
    root: &'a Value,
    /// References being expanded on the current path, to stop at recursive types.
    resolving: HashSet<String>,
    expansions: usize,
    /// Whether property comments include constraint tags.
    tags: bool,
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

/// Draft-04 and OpenAPI 3.0 make `minimum`/`maximum` exclusive with a boolean `exclusiveMinimum`/`exclusiveMaximum`.
fn with_numeric_exclusives(schema: &Map<String, Value>) -> Map<String, Value> {
    let mut schema = schema.clone();
    for (exclusive, bound) in [("exclusiveMinimum", "minimum"), ("exclusiveMaximum", "maximum")] {
        if schema.get(exclusive) == Some(&Value::Bool(true)) {
            match schema.remove(bound) {
                Some(value) => schema.insert(exclusive.into(), value),
                None => schema.remove(exclusive),
            };
        }
    }
    schema
}

/// Constraints of a property that its TypeScript type cannot express, as JSDoc-style tags:
/// `@integer @minimum 1 @maximum 100 @default 20`. Empty when there are none.
fn constraint_tags(property: &Value) -> String {
    let Some(property) = property.as_object() else { return String::new() };
    // Optional fields such as Pydantic's `int | None` keep their constraints in the non-null branch.
    // The branch's constraints apply together with the property's, so both are read.
    let variants = match (property.get("anyOf"), property.get("oneOf")) {
        (Some(Value::Array(variants)), _) | (_, Some(Value::Array(variants))) => variants.as_slice(),
        _ => &[],
    };
    let non_null: Vec<&Value> = variants
        .iter()
        .filter(|v| v.as_object().is_none_or(|v| v.get("type").and_then(Value::as_str) != Some("null")))
        .collect();
    let mut schemas = vec![with_numeric_exclusives(property)];
    if let [Value::Object(branch)] = non_null.as_slice() {
        schemas.push(with_numeric_exclusives(branch));
    }

    let mut tags: Vec<String> = Vec::new();
    let is_integer = |schema: &Map<String, Value>| match schema.get("type") {
        Some(Value::String(kind)) => kind == "integer",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("integer")),
        _ => false,
    };
    if schemas.iter().any(is_integer) {
        tags.push("@integer".into());
    }
    for keyword in TAG_KEYWORDS {
        let values: Vec<&Value> = schemas.iter().filter_map(|schema| schema.get(keyword)).collect();
        let lower = LOWER_BOUNDS.contains(&keyword);
        if lower || UPPER_BOUNDS.contains(&keyword) {
            let numbers = values.iter().filter_map(|value| value.as_f64());
            let bound = if lower { numbers.reduce(f64::max) } else { numbers.reduce(f64::min) };
            if let Some(bound) = bound {
                tags.push(format!("@{keyword} {}", js_number(bound)));
            }
            continue;
        }
        let mut seen: Vec<(bool, String)> = Vec::new();
        for value in values {
            let key = match value {
                Value::Number(number) => (true, js_number(number.as_f64().unwrap_or(f64::NAN))),
                Value::String(text) => (false, text.clone()),
                _ => continue,
            };
            if !seen.contains(&key) {
                tags.push(format!("@{keyword} {}", key.1));
                seen.push(key);
            }
        }
    }
    if schemas.iter().any(|schema| schema.get("uniqueItems") == Some(&Value::Bool(true))) {
        tags.push("@uniqueItems".into());
    }
    // Annotations, unlike constraints, come from the property itself.
    if let Some(default) = property.get("default").filter(|default| !default.is_null()) {
        tags.push(format!("@default {}", js_stringify(default)));
    }
    if property.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".into());
    }
    let escape = |tag: String| {
        tag.chars()
            .map(|c| if is_line_terminator(c) { format!("\\u{:04x}", u32::from(c)) } else { c.to_string() })
            .collect::<String>()
    };
    tags.into_iter().filter(|tag| utf16_len(tag) <= MAX_TAG_CHARS).map(escape).collect::<Vec<_>>().join(" ")
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
    let comments: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            let property = &properties[name.as_str()];
            let mut lines: Vec<String> = description_of(property)
                .split(is_line_terminator)
                .map(js_trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            let tags = if context.tags { constraint_tags(property) } else { String::new() };
            if !tags.is_empty() {
                lines.push(tags);
            }
            lines
        })
        .collect();
    if comments.iter().all(Vec::is_empty) {
        return format!("{{ {} }}", members.join(" "));
    }

    let mut lines = vec!["{".to_string()];
    for (index, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(format!("{INDENT}// {line}"));
        }
        lines.push(format!("{INDENT}{}", members[index].replace('\n', &format!("\n{INDENT}"))));
    }
    for member in &members[names.len()..] {
        lines.push(format!("{INDENT}{member}"));
    }
    lines.push("}".into());
    lines.join("\n")
}
