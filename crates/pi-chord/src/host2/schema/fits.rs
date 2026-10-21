//! Upstream `fits`: conservative structural subtyping between schemas, the core of contract coverage.

use super::{as_schema, deep_equal, deep_equal_opt};
use crate::json::{Json, JsonMap};

const CONSTRAINTS: [&str; 14] = [
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];

/// Keywords `fits` reasons about besides the constraints. Any other validation keyword in the
/// provider's schema forces exact equality.
const KNOWN: [&str; 12] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "patternProperties",
    "items",
    "additionalItems",
    "const",
    "enum",
    "anyOf",
    "parameters",
    "returnType",
];

/// Annotations: no effect on validity.
const ANNOTATIONS: [&str; 10] = [
    "title",
    "description",
    "default",
    "examples",
    "$id",
    "$schema",
    "$comment",
    "readOnly",
    "writeOnly",
    "deprecated",
];

fn without_annotations(schema: Option<&Json>) -> Json {
    let schema = as_schema(schema);
    let kept = schema.iter().filter(|(key, _)| !ANNOTATIONS.contains(&key.as_ref()));
    Json::object(kept.map(|(key, value)| (key.clone(), value.clone())).collect())
}

fn types_of(schema: &JsonMap) -> Option<Vec<&Json>> {
    match schema.get("type")? {
        kind @ Json::String(_) => Some(vec![kind]),
        Json::Array(kinds) => Some(kinds.iter().collect()),
        _ => None,
    }
}

fn includes(types: &[&Json], kind: &str) -> bool {
    types.iter().any(|item| item.as_str() == Some(kind))
}

/// Conservative structural subtyping: true only if every value valid under `sub` is valid under `sup`.
/// Object fields missing from `sub` but optional in `sup` are fine. A provider keyword this does not
/// model (`not`, `allOf`, `$ref`, ...) or an `additionalProperties` schema needs an identical schema.
pub fn fits(sub: &Json, sup: &Json) -> bool {
    fits_opt(Some(sub), Some(sup))
}

fn fits_opt(sub: Option<&Json>, sup: Option<&Json>) -> bool {
    let sub_json = without_annotations(sub);
    let sup_json = without_annotations(sup);
    let (Some(sub), Some(sup)) = (sub_json.as_object(), sup_json.as_object()) else { return false };
    if sup.is_empty() || deep_equal(&sub_json, &sup_json) {
        return true;
    }
    let known = |key: &str| CONSTRAINTS.contains(&key) || KNOWN.contains(&key);
    if sup.keys().any(|key| !known(key)) {
        return false;
    }
    if sup.get("additionalProperties").is_some_and(|additional| !matches!(additional, Json::Bool(_))) {
        return false;
    }
    if let Some(Json::Array(branches)) = sub.get("anyOf") {
        return branches.iter().all(|branch| fits_opt(Some(branch), Some(&sup_json)));
    }
    if let Some(Json::Array(branches)) = sup.get("anyOf") {
        return branches.iter().any(|branch| fits_opt(Some(&sub_json), Some(branch)));
    }
    if let Some(expected) = sup.get("const") {
        return sub.get("const").is_some_and(|actual| deep_equal(actual, expected));
    }
    if let Some(Json::Array(allowed)) = sup.get("enum") {
        let values: Vec<&Json> = match (sub.get("const"), sub.get("enum")) {
            (Some(constant), _) => vec![constant],
            (None, Some(Json::Array(values))) => values.iter().collect(),
            _ => return false,
        };
        return values.iter().all(|item| allowed.iter().any(|option| deep_equal(option, item)));
    }
    if CONSTRAINTS.iter().any(|key| sup.contains_key(key) && !deep_equal_opt(sub.get(key), sup.get(key))) {
        return false;
    }
    let (Some(sup_types), Some(sub_types)) = (types_of(sup), types_of(sub)) else { return false };
    let covered =
        |kind: &&Json| sup_types.contains(kind) || (kind.as_str() == Some("integer") && includes(&sup_types, "number"));
    if !sub_types.iter().all(covered) {
        return false;
    }
    if sub.contains_key("const") || matches!(sub.get("enum"), Some(Json::Array(_))) {
        return true;
    }
    if includes(&sub_types, "object") && !object_fits(sub, sup) {
        return false;
    }
    if includes(&sub_types, "array") && !array_fits(sub, sup) {
        return false;
    }
    if includes(&sub_types, "function") {
        return function_fits(sub, sup);
    }
    true
}

fn list(value: Option<&Json>) -> &[Json] {
    match value {
        Some(Json::Array(items)) => items,
        _ => &[],
    }
}

/// JavaScript `typeof value === "object"`: objects, arrays and `null`.
fn object_like(value: Option<&Json>) -> bool {
    matches!(value, Some(Json::Object(_) | Json::Array(_) | Json::Null))
}

fn object_fits(sub: &JsonMap, sup: &JsonMap) -> bool {
    let sub_properties = as_schema(sub.get("properties"));
    let sup_properties = as_schema(sup.get("properties"));
    let sub_required = list(sub.get("required"));
    if !list(sup.get("required")).iter().all(|key| sub_required.contains(key)) {
        return false;
    }
    let additional = sub.get("additionalProperties");
    for (key, property) in sup_properties.iter() {
        let fit = match sub_properties.get(key) {
            Some(own) => fits(own, property),
            // The consumer may send this name with any value its `additionalProperties` allows.
            None if object_like(additional) => fits_opt(additional, Some(property)),
            None => true,
        };
        if !fit {
            return false;
        }
    }
    let closed = |schema: &JsonMap| schema.get("additionalProperties") == Some(&Json::Bool(false));
    if closed(sup) && (!closed(sub) || sub_properties.keys().any(|key| !sup_properties.contains_key(key))) {
        return false;
    }
    patterns_fit(sub, sup, &sub_properties, &sup_properties)
}

/// Every value the consumer may put under a matching name must fit, and the consumer must not allow
/// arbitrary extra names (it would then allow any value under a matching one).
fn patterns_fit(sub: &JsonMap, sup: &JsonMap, sub_properties: &JsonMap, sup_properties: &JsonMap) -> bool {
    let sup_patterns = as_schema(sup.get("patternProperties"));
    if sup_patterns.is_empty() {
        return true;
    }
    let sub_patterns = as_schema(sub.get("patternProperties"));
    if sub.get("additionalProperties") != Some(&Json::Bool(false)) && sub_patterns.is_empty() {
        return false;
    }
    for (pattern, property) in sup_patterns.iter() {
        // An invalid pattern throws upstream; here it refuses the fit.
        if pi_schema::pattern_test(pattern, "").is_none() {
            return false;
        }
        for (key, own) in sub_properties.iter().filter(|(key, _)| !sup_properties.contains_key(key)) {
            if pi_schema::pattern_test(pattern, key) == Some(true) && !fits(own, property) {
                return false;
            }
        }
        if !sub_patterns.is_empty() && !sub_patterns.get(pattern).is_some_and(|own| fits(own, property)) {
            return false;
        }
    }
    sub_patterns.keys().all(|pattern| sup_patterns.contains_key(pattern))
}

fn array_fits(sub: &JsonMap, sup: &JsonMap) -> bool {
    match (sub.get("items"), sup.get("items")) {
        (Some(Json::Array(own)), Some(Json::Array(other))) => {
            own.len() == other.len() && own.iter().zip(other.iter()).all(|(own, other)| fits(own, other))
        }
        (_, Some(Json::Array(_))) => false,
        (Some(Json::Array(own)), other) => own.iter().all(|own| fits_opt(Some(own), other)),
        (own, other) => fits_opt(own, other),
    }
}

/// A function accepting fewer parameters is fine; each shared parameter is contravariant.
fn function_fits(sub: &JsonMap, sup: &JsonMap) -> bool {
    let sub_parameters = list(sub.get("parameters"));
    let sup_parameters = list(sup.get("parameters"));
    if sub_parameters.len() > sup_parameters.len() {
        return false;
    }
    if !sub_parameters.iter().zip(sup_parameters.iter()).all(|(own, other)| fits(other, own)) {
        return false;
    }
    fits_opt(sub.get("returnType"), sup.get("returnType"))
}
