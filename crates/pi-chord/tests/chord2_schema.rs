//! The chord2 schema module (upstream `schema.ts`) against golden fixtures from upstream TypeScript: hash64,
//! canonical JSON, `fits`, `normalize` with its identity, and `assertValid` messages. Also ports review test #11.

mod chord2_support;

use chord2_support::*;
use pi_chord::host2::schema::{assert_valid, canonical_identity, canonical_json, fits, hash64, normalize};
use pi_chord::{ChordError, Json, json};

fn error_json(error: &ChordError) -> Json {
    let code = error.code().map(|code| code.as_str()).unwrap_or("none");
    json!({"code": code, "reason": error.reason(), "message": error.to_string()})
}

#[test]
fn hash64_and_canonical_json_match_upstream() {
    let fixture = fixture("hash.json");
    for case in fixture["hash64"].as_array().unwrap() {
        assert_eq!(hash64(text(&case["text"])), text(&case["hash"]), "{}", case["text"]);
    }
    for case in fixture["canonical"].as_array().unwrap() {
        assert_eq!(canonical_json(&case["value"]), text(&case["canonical"]), "{}", case["value"]);
    }
}

#[test]
fn canonical_json_sorts_keys_by_utf16_code_units() {
    // U+FFFF sorts after U+1F600 by code point, before it by UTF-16 code units (a surrogate pair).
    let value = json!({"\u{ffff}": 1, "\u{1f600}": 2, "b": 3, "10": 4, "9": 5});
    assert_eq!(canonical_json(&value), "{\"10\":4,\"9\":5,\"b\":3,\"\u{1f600}\":2,\"\u{ffff}\":1}");
}

#[test]
fn fits_matches_upstream() {
    let fixture = fixture("schema.json");
    for case in fixture["fits"].as_array().unwrap() {
        let expected = case["fits"].as_bool().unwrap();
        assert_eq!(fits(&case["sub"], &case["sup"]), expected, "fits({}, {})", case["sub"], case["sup"]);
    }
}

#[test]
fn normalize_and_identity_match_upstream() {
    let fixture = fixture("schema.json");
    for case in fixture["normalize"].as_array().unwrap() {
        let label = format!("normalize({}, {})", case["schema"], case["input"]);
        match normalize(&case["schema"], Some(&case["input"]), "input") {
            Ok(value) => {
                // Text comparison: key order is part of the result (defaults are appended).
                assert_eq!(value.to_string(), case["value"].to_string(), "{label}");
                assert_eq!(canonical_identity(&case["schema"], &value), text(&case["identity"]), "{label}");
            }
            Err(error) => assert_eq!(error_json(&error), case["error"], "{label}"),
        }
    }
}

#[test]
fn assert_valid_messages_match_upstream() {
    let fixture = fixture("schema.json");
    for case in fixture["assertValid"].as_array().unwrap() {
        let label = format!("assertValid({}, {})", case["schema"], case["value"]);
        let actual = match assert_valid("state", "state s", &case["schema"], Some(&case["value"])) {
            Ok(()) => Json::Null,
            Err(error) => error_json(&error),
        };
        assert_eq!(actual, case["error"], "{label}");
    }
}

#[test]
fn assert_valid_reports_details_and_missing_values() {
    let schema = object(&[req("a", string()), req("n", number())]);
    let error = assert_valid("args", "t.x.m args", &schema, Some(&json!({"a": 1, "n": "x"}))).unwrap_err();
    assert_eq!(
        error.details().cloned(),
        Some(json!([
            {"keyword": "type", "instancePath": "/a", "message": "must be string"},
            {"keyword": "type", "instancePath": "/n", "message": "must be number"}
        ]))
    );
    // A missing value (`undefined`) passes only schemas that allow it.
    assert!(assert_valid("result", "r", &unknown(), None).is_ok());
    assert!(assert_valid("result", "r", &void(), None).is_ok());
    let error = assert_valid("result", "r", &string(), None).unwrap_err();
    assert_eq!(error.to_string(), "invalid/result: r: / must be string");
}

/// Upstream `review.test.ts` #11: no false positives for unmodelled keywords; each counterexample is valid under
/// the consumer's schema and invalid under the provider's.
#[test]
fn fits_has_no_false_positives_for_unmodelled_keywords() {
    let cases = [
        (string(), with(string(), serde_json::json!({"not": {"const": "x"}})), json!("x")),
        (array(number()), with(array(number()), serde_json::json!({"contains": {"const": 1}})), json!([2])),
        (
            object(&[req("a", string())]),
            with(object(&[req("a", string())]), serde_json::json!({"additionalProperties": {"type": "number"}})),
            json!({"a": "s", "b": "t"}),
        ),
        (
            object(&[req("x", string())]),
            with(object(&[]), serde_json::json!({"patternProperties": {"^x$": {"type": "number"}}})),
            json!({"x": "s"}),
        ),
        (number(), json!({"allOf": [{"type": "number"}, {"type": "number", "minimum": 1}]}), json!(0)),
        (
            with(object(&[req("a", string())]), serde_json::json!({"additionalProperties": {"type": "number"}})),
            object(&[req("a", string()), opt("b", string())]),
            json!({"a": "", "b": 1}),
        ),
    ];
    for (sub, sup, value) in cases {
        let check = |schema: &Json| pi_schema::check(&schema.to_serde(), &value.to_serde());
        assert!(check(&sub) && !check(&sup), "bad counterexample {value}");
        assert!(!fits(&sub, &sup), "fits({sub}, {sup})");
    }
    // Additive changes still fit.
    assert!(fits(&object(&[req("a", string())]), &object(&[req("a", string()), opt("b", number())])));
    assert!(fits(&object(&[req("a", string()), req("extra", number())]), &object(&[req("a", string())])));
}
