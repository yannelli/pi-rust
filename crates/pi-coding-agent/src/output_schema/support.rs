//! Up-front check that an `--output-schema` file is a schema `pi-schema` can enforce. The
//! checker skips what it does not understand, so this reports it instead: non-schema values,
//! unknown types, unresolved or remote `$ref`s, bad patterns, and unchecked keywords.

use std::collections::HashSet;

use serde_json::{Map, Value};

/// Keywords whose value is one subschema.
const SINGLE: &[&str] =
    &["additionalProperties", "additionalItems", "contains", "propertyNames", "not", "if", "then", "else"];
/// Keywords whose value is an array of subschemas.
const LISTS: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];
/// Keywords whose value maps names to subschemas.
const MAPS: &[&str] = &["properties", "patternProperties", "$defs", "definitions"];
/// Keywords `pi-schema` does not check, so a schema with them would accept too much.
const UNCHECKED: &[&str] =
    &["unevaluatedProperties", "unevaluatedItems", "dependentSchemas", "dependencies", "$dynamicRef", "$recursiveRef"];
const TYPES: &[&str] = &["string", "number", "integer", "boolean", "object", "array", "null"];
/// Problems listed at most.
const MAX_PROBLEMS: usize = 8;

/// What makes `schema` unusable, one `#/pointer: problem` line each; empty when it is usable.
pub(super) fn unsupported(schema: &Value) -> Vec<String> {
    if !schema.is_object() {
        return vec!["#: the schema must be a JSON object".into()];
    }
    let mut walk = Walk { root: schema, problems: Vec::new(), nodes: Vec::new() };
    walk.schema(schema, "#".into());
    if walk.problems.is_empty() {
        let mut done = HashSet::new();
        let cycle = walk.nodes.iter().find_map(|node| in_place_cycle(schema, node, &mut Vec::new(), &mut done));
        walk.problems.extend(cycle.map(|pointer| format!("{pointer}: $ref refers to itself without nesting")));
    }
    walk.problems.truncate(MAX_PROBLEMS);
    walk.problems
}

/// The subschema a local `$ref` such as `#/$defs/item` names.
fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    root.pointer(reference.strip_prefix('#')?)
}

fn child(pointer: &str, token: &str) -> String {
    format!("{pointer}/{}", token.replace('~', "~0").replace('/', "~1"))
}

struct Walk<'a> {
    root: &'a Value,
    problems: Vec<String>,
    /// Pointers of every subschema, for the cycle check.
    nodes: Vec<String>,
}

impl Walk<'_> {
    fn problem(&mut self, pointer: &str, text: impl std::fmt::Display) {
        self.problems.push(format!("{pointer}: {text}"));
    }

    fn schema(&mut self, node: &Value, pointer: String) {
        let map = match node {
            Value::Bool(_) => return,
            Value::Object(map) => map,
            _ => return self.problem(&pointer, "a schema must be an object or a boolean"),
        };
        for keyword in UNCHECKED.iter().filter(|keyword| map.contains_key(**keyword)) {
            self.problem(&pointer, format!("\"{keyword}\" is not supported"));
        }
        self.types(map, &pointer);
        match map.get("$ref") {
            Some(Value::String(reference)) if resolve(self.root, reference).is_some() => {}
            Some(reference) => {
                self.problem(&child(&pointer, "$ref"), format!("{reference} is not a local reference into this file"))
            }
            None => {}
        }
        match map.get("pattern") {
            Some(Value::String(pattern)) => self.pattern(&child(&pointer, "pattern"), pattern),
            Some(_) => self.problem(&child(&pointer, "pattern"), "must be a string"),
            None => {}
        }
        self.nodes.push(pointer.clone());
        self.children(map, &pointer);
    }

    fn types(&mut self, map: &Map<String, Value>, pointer: &str) {
        let kinds: Vec<&Value> = match map.get("type") {
            None => return,
            Some(Value::Array(kinds)) => kinds.iter().collect(),
            Some(kind) => vec![kind],
        };
        for kind in kinds {
            if !kind.as_str().is_some_and(|kind| TYPES.contains(&kind)) {
                self.problem(&child(pointer, "type"), format!("unknown type {kind}"));
            }
        }
    }

    fn pattern(&mut self, pointer: &str, pattern: &str) {
        if let Err(error) = regex::Regex::new(pattern) {
            let first = error.to_string().lines().last().unwrap_or_default().trim().to_string();
            self.problem(pointer, format!("pattern {pattern:?} does not compile: {first}"));
        }
    }

    fn children(&mut self, map: &Map<String, Value>, pointer: &str) {
        for keyword in SINGLE {
            if let Some(node) = map.get(*keyword) {
                self.schema(node, child(pointer, keyword));
            }
        }
        match map.get("items") {
            Some(Value::Array(items)) => self.list(items, &child(pointer, "items")),
            Some(node) => self.schema(node, child(pointer, "items")),
            None => {}
        }
        for keyword in LISTS {
            match map.get(*keyword) {
                Some(Value::Array(items)) => self.list(items, &child(pointer, keyword)),
                Some(_) => self.problem(&child(pointer, keyword), "must be an array of schemas"),
                None => {}
            }
        }
        for keyword in MAPS {
            let at = child(pointer, keyword);
            match map.get(*keyword) {
                Some(Value::Object(entries)) => {
                    for (name, node) in entries {
                        if *keyword == "patternProperties" {
                            self.pattern(&child(&at, name), name);
                        }
                        self.schema(node, child(&at, name));
                    }
                }
                Some(_) => self.problem(&at, "must be an object of schemas"),
                None => {}
            }
        }
    }

    fn list(&mut self, items: &[Value], pointer: &str) {
        for (index, node) in items.iter().enumerate() {
            self.schema(node, child(pointer, &index.to_string()));
        }
    }
}

/// A subschema that checking reaches again for the same value (through `$ref`, `allOf`,
/// `anyOf`, `oneOf`, `not`, and `if`/`then`/`else`), which would recurse forever.
fn in_place_cycle(root: &Value, pointer: &str, path: &mut Vec<String>, done: &mut HashSet<String>) -> Option<String> {
    if path.iter().any(|seen| seen == pointer) {
        return Some(pointer.to_string());
    }
    if done.contains(pointer) {
        return None;
    }
    let Some(Value::Object(map)) = resolve(root, pointer) else { return None };
    let mut next: Vec<String> = map.get("$ref").and_then(Value::as_str).map(String::from).into_iter().collect();
    for keyword in ["not", "if", "then", "else"].into_iter().filter(|keyword| map.contains_key(*keyword)) {
        next.push(child(pointer, keyword));
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        let count = map.get(keyword).and_then(Value::as_array).map_or(0, Vec::len);
        next.extend((0..count).map(|index| child(&child(pointer, keyword), &index.to_string())));
    }
    path.push(pointer.to_string());
    let found = next.iter().find_map(|target| in_place_cycle(root, target, path, done));
    path.pop();
    done.insert(pointer.to_string());
    found
}
