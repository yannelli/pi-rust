//! Ported from upstream `test/streaming-json.test.ts`: strings, partial-JSON test vectors,
//! malformed text, changes, cost, and `ToolCallArgumentsStream`.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::tests_fuzz::reference_snapshot;
use super::*;
use crate::json::parse_streaming_json;

/// `text` in pieces of `size` characters.
pub(super) fn pieces(text: &str, size: usize) -> Vec<&str> {
    let mut bounds: Vec<usize> = text.char_indices().map(|(index, _)| index).step_by(size.max(1)).collect();
    bounds.push(text.len());
    bounds.windows(2).map(|bound| &text[bound[0]..bound[1]]).filter(|piece| !piece.is_empty()).collect()
}

/// The snapshot after reading `text` in pieces of `size` characters, taking a snapshot after each.
pub(super) fn final_snapshot(text: &str, size: usize) -> Option<Value> {
    let mut parser = StreamingJsonParser::new();
    for piece in pieces(text, size) {
        parser.append(piece);
        parser.snapshot();
    }
    parser.snapshot().cloned()
}

fn whole(text: &str) -> Option<Value> {
    final_snapshot(text, text.chars().count())
}

fn path(spec: &str) -> JsonPath {
    let segment = |part: &str| match part.parse() {
        Ok(index) => PathSegment::Index(index),
        Err(_) => PathSegment::Key(part.to_string()),
    };
    spec.split('/').filter(|part| !part.is_empty()).map(segment).collect()
}

fn set(spec: &str, value: Value) -> JsonChange {
    JsonChange::Set { path: path(spec), value }
}

fn append(spec: &str, text: &str) -> JsonChange {
    JsonChange::Append { path: path(spec), text: text.into() }
}

#[test]
fn holds_an_escape_back_until_it_is_complete_at_every_split_position() {
    for (text, value) in [
        (r#"{"a":"x\u00e9y"}"#, "x\u{e9}y"),
        (r#"{"a":"x\ny"}"#, "x\ny"),
        (r#"{"a":"x\\y"}"#, "x\\y"),
        (r#"{"a":"x\"y"}"#, "x\"y"),
        (r#"{"a":"x\/y"}"#, "x/y"),
    ] {
        for split in 0..=text.len() {
            let mut parser = StreamingJsonParser::new();
            parser.append(&text[..split]);
            assert_eq!(parser.snapshot().cloned(), reference_snapshot(&text[..split], true), "{}", &text[..split]);
            parser.append(&text[split..]);
            assert_eq!(parser.snapshot(), Some(&json!({"a": value})));
        }
    }
}

#[test]
fn joins_surrogate_pairs_split_across_pieces() {
    for text in [r#"{"a":"\ud83d\ude00!"}"#, "{\"a\":\"\u{1f600}!\"}"] {
        for split in (0..=text.len()).filter(|split| text.is_char_boundary(*split)) {
            let mut parser = StreamingJsonParser::new();
            parser.append(&text[..split]);
            parser.snapshot();
            parser.append(&text[split..]);
            assert_eq!(parser.snapshot(), Some(&json!({"a": "\u{1f600}!"})));
        }
    }
    // Rust strings cannot hold a lone surrogate: a high half waits for its low half, and an
    // unpaired one is dropped, as repair_json() drops it.
    let mut parser = StreamingJsonParser::new();
    parser.append(r#"{"a":"\ud83d"#);
    assert_eq!(parser.snapshot(), Some(&json!({"a": ""})));
    parser.append(r#"x\ude00y\ud83d\n"}"#);
    assert_eq!(parser.snapshot(), Some(&json!({"a": "xy\n"})));
}

#[test]
fn keeps_trailing_whitespace_raw_control_characters_and_invalid_escapes_visible() {
    let mut parser = StreamingJsonParser::new();
    parser.append("{\"code\":\"x\t\t  ");
    assert_eq!(parser.snapshot(), Some(&json!({"code": "x\t\t  "})));
    let mut parser = StreamingJsonParser::new();
    parser.append("{\"a\":\"x\ny \\d \\u12\"");
    assert_eq!(parser.snapshot(), Some(&json!({"a": "x\ny \\d \\u12"})));
}

#[test]
fn keeps_invalid_unicode_escapes_literal_without_dropping_fields() {
    let text = r#"{"path":"a.tex","content":"\usepackage{x}","mode":"w"}"#;
    assert_eq!(final_snapshot(text, 1), Some(json!({"path": "a.tex", "content": "\\usepackage{x}", "mode": "w"})));
    assert_eq!(final_snapshot(r#"{"text":"caf\u00e9 \url"}"#, 3), Some(json!({"text": "caf\u{e9} \\url"})));
    let mut parser = StreamingJsonParser::new();
    parser.append(r#"{"text":"ab\u00"#);
    assert_eq!(parser.snapshot(), Some(&json!({"text": "ab"})));
}

#[test]
fn keeps_the_last_value_of_a_duplicate_key_in_the_first_keys_position() {
    let text = r#"{"a":1,"b":2,"a":"three"}"#;
    let expected: Value = serde_json::from_str(text).unwrap();
    let snapshot = final_snapshot(text, 1).unwrap();
    let keys = |value: &Value| value.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&snapshot), keys(&expected));
    assert_eq!(snapshot, expected);
}

/// Inputs and expected values from other partial-JSON parsers' public test suites: partial-json
/// (MIT, promplate/partial-json-parser-js) and the Vercel AI SDK `fixJson` (Apache-2.0).
fn vectors() -> Vec<(&'static str, Option<Value>)> {
    let mut vectors = vec![
        ("\"", Some(json!(""))),
        ("[\"", Some(json!([""]))),
        ("{\"\": \"", Some(json!({"": ""}))),
        ("n", Some(json!(null))),
        ("t", Some(json!(true))),
        ("f", Some(json!(false))),
        ("0", Some(json!(0))),
        ("-1.25e+4", Some(json!(-1.25e4))),
        ("-1.25e+", Some(json!(-1.25))),
        ("-1.25e", Some(json!(-1.25))),
        // An invalid escape stays visible, and `Infinity`/`NaN` words are not JSON and are dropped.
        ("\" \\x12", Some(json!(" \\x12"))),
        ("I", None),
        ("-I", None),
        ("N", None),
    ];
    let fix_json = [
        ("", None),
        ("nul", Some(json!(null))),
        ("fals", Some(json!(false))),
        ("12.2", Some(json!(12.2))),
        ("-12", Some(json!(-12))),
        ("-", None),
        ("2.5e", Some(json!(2.5))),
        ("2.5e-", Some(json!(2.5))),
        ("2.5e3", Some(json!(2.5e3))),
        ("-2.5E3", Some(json!(-2.5e3))),
        ("2.5E-", Some(json!(2.5))),
        ("5e", Some(json!(5))),
        ("\"abc", Some(json!("abc"))),
        ("\"value with \\\"quoted\\\" text and \\\\ escape", Some(json!("value with \"quoted\" text and \\ escape"))),
        ("\"value with \\", Some(json!("value with "))),
        ("\"\\u", Some(json!(""))),
        ("\"\\u12", Some(json!(""))),
        ("\"text \\u00", Some(json!("text "))),
        ("{\"a\":\"\\u12", Some(json!({"a": ""}))),
        ("\"value with unicode \\u003C\"", Some(json!("value with unicode <"))),
        ("[", Some(json!([]))),
        ("[[1], [2", Some(json!([[1], [2]]))),
        ("[[\"1\"], [\"2", Some(json!([["1"], ["2"]]))),
        ("[[false], [nu", Some(json!([[false], [null]]))),
        ("[[[]], [[]", Some(json!([[[]], [[]]]))),
        ("[[{}], [{", Some(json!([[{}], [{}]]))),
        ("[1, ", Some(json!([1]))),
        ("[[], 123", Some(json!([[], 123]))),
        ("{\"key\":", Some(json!({}))),
        ("{\"a\": {\"b\": 1}, \"c\": {\"d\": 2", Some(json!({"a": {"b": 1}, "c": {"d": 2}}))),
        ("{\"a\": {\"b\": []}, \"c\": {\"d\": 2", Some(json!({"a": {"b": []}, "c": {"d": 2}}))),
        ("{\"ke", Some(json!({}))),
        ("{\"k1\": 1, \"k2", Some(json!({"k1": 1}))),
        ("{\"k1\": 1, \"k2\":", Some(json!({"k1": 1}))),
        ("{\"key\": \"value\"  ", Some(json!({"key": "value"}))),
        ("[1, [2, 3, [", Some(json!([1, [2, 3, []]]))),
        ("{\"key\": {\"subKey\":", Some(json!({"key": {}}))),
        ("{\"key\": [1, 2, {", Some(json!({"key": [1, 2, {}]}))),
        ("[1, 2, {\"key\": \"value\",", Some(json!([1, 2, {"key": "value"}]))),
        ("{\"a\": {\"b\": [\"c\", {\"d\": \"e\",", Some(json!({"a": {"b": ["c", {"d": "e"}]}}))),
        ("{\"a\": 1, \"b\": \"", Some(json!({"a": 1, "b": ""}))),
        (
            "{\n  \"a\": [\n    {\n      \"a1\": \"v1\",\n      \"a2\": \"v2\"\n    }\n  ],\n  \"b\": [\n    {\n      \"b1\": \"n",
            Some(json!({"a": [{"a1": "v1", "a2": "v2"}], "b": [{"b1": "n"}]})),
        ),
        // fixJson completes `12.` to 12; like parse_streaming_json, a number cut after its dot is omitted.
        ("12.", None),
        ("12.e", None),
    ];
    vectors.extend(fix_json);
    vectors
}

#[test]
fn reads_ported_test_vectors_at_every_chunk_size() {
    for (input, expected) in vectors() {
        for size in [input.len().max(1), 1, 2] {
            assert_eq!(final_snapshot(input, size), expected, "{input:?} in pieces of {size}");
        }
    }
}

const MALFORMED: [&str; 17] = [
    r#"{"a":1 "b":2}"#,
    r#"{"a" 1, "b": 2}"#,
    "{a:1, 'b': 2}",
    r#"{"a":1,,"b":2,}"#,
    "[1 2, , 3,]",
    r#"{"a":[1,2}, "b":3]"#,
    "]]]}}}{",
    r#"{"a":NaN,"b":Infinity,"c":-Infinity,"d":undefined,"e":1}"#,
    r#"{"a":01,"b":1.,"c":.5,"d":+1,"e":0x10,"f":1e}"#,
    r#"{"a":"x"} trailing garbage {"b":2}"#,
    r#"{"a": tru, "b": nulll, "c": falsey}"#,
    r#"{"a":{"b":{"c":"#,
    r#"{"a\": 1}"#,
    r#""unterminated \"#,
    "// comment\n{\"a\":1}",
    "\u{0}{\u{1}\"a\"\u{2}:\u{3}1}",
    r#"{"a":"x"}}}]]]"#,
];

#[test]
fn reads_malformed_text_the_same_way_at_every_chunk_size() {
    for text in MALFORMED {
        for size in [1, 2, 3, 7] {
            assert_eq!(final_snapshot(text, size), whole(text), "{text:?} in pieces of {size}");
        }
    }
}

#[test]
fn tolerates_missing_commas_and_colons_and_drops_values_in_invalid_positions() {
    assert_eq!(whole(r#"{"a":1 "b":2}"#), Some(json!({"a": 1, "b": 2})));
    assert_eq!(whole(r#"{"a" 1, "b": 2}"#), Some(json!({"a": 1, "b": 2})));
    assert_eq!(whole("[1 2, , 3,]"), Some(json!([1, 2, 3])));
    assert_eq!(whole(r#"{"a":NaN,"b":Infinity,"e":1}"#), Some(json!({"e": 1})));
    assert_eq!(whole(r#"{"a":"x"} trailing garbage {"b":2}"#), Some(json!({"a": "x"})));
    assert_eq!(whole(r#"{"a":1 {"b":[2,{"c":3}]} ,"d":4}"#), Some(json!({"a": 1, "d": 4})));
}

#[test]
fn reports_a_number_that_ends_and_a_string_that_starts_in_one_piece_in_order() {
    let mut parser = StreamingJsonParser::new();
    assert_eq!(parser.append(r#"{"n":1"#), [set("", json!({}))]);
    assert_eq!(parser.append(r#"2,"s":"ab"#), [set("n", json!(12)), set("s", json!("ab"))]);
    assert_eq!(parser.append(r#"c","t":tru"#), [append("s", "c")]);
    assert_eq!(parser.append("e}"), [set("t", json!(true))]);
    assert_eq!(parser.append(r#" {"ignored":1}"#), []);
}

#[test]
fn reports_nested_containers_empty_then_their_members_and_items() {
    let mut parser = StreamingJsonParser::new();
    assert_eq!(
        parser.append(r#"{"a":[1,{"b":["x""#),
        [
            set("", json!({})),
            set("a", json!([])),
            set("a/0", json!(1)),
            set("a/1", json!({})),
            set("a/1/b", json!([])),
            set("a/1/b/0", json!("x")),
        ]
    );
    assert_eq!(parser.append(r#","y"#), [set("a/1/b/1", json!("y"))]);
    assert_eq!(parser.append(r#"z"]}],"a":"again"}"#), [append("a/1/b/1", "z"), set("a", json!("again"))]);
}

#[test]
fn holds_split_escapes_back_and_reports_their_decoded_text_once_complete() {
    let text = r#"{"a":"x\u00e9\n\\\"\ud83d\ude00\q"}"#;
    let expected: Value = serde_json::from_str(r#"{"a":"x\u00e9\n\\\"\ud83d\ude00\\q"}"#).unwrap();
    for first in 1..text.len() {
        for second in first..text.len() {
            let mut parser = StreamingJsonParser::new();
            let mut mirror = None;
            for piece in [&text[..first], &text[first..second], &text[second..]] {
                parser.append(piece).iter().for_each(|change| change.apply(&mut mirror));
            }
            assert_eq!(mirror.as_ref(), Some(&expected));
        }
    }
    let mut parser = StreamingJsonParser::new();
    parser.append(r#"{"a":"x"#);
    assert_eq!(parser.append(r"\u00"), []);
    assert_eq!(parser.append("e9"), [append("a", "\u{e9}")]);
}

#[test]
fn reports_a_container_nested_deeper_than_32_levels_once_whole_when_it_closes() {
    let text = format!("{}\"deep\"{}", "{\"a\":".repeat(40), "}".repeat(40));
    let expected: Value = serde_json::from_str(&text).unwrap();
    let (mut parser, mut mirror) = (StreamingJsonParser::new(), None);
    for piece in pieces(&text, 3) {
        parser.append(piece).iter().for_each(|change| change.apply(&mut mirror));
    }
    assert_eq!(mirror, Some(expected));
    let changes = StreamingJsonParser::new().append(&text).to_vec();
    // The containers down to path length 32, then the one at length 32 again, whole, as it closes.
    assert_eq!(changes.len(), 34);
    let Some(JsonChange::Set { path, value }) = changes.last() else { panic!("{changes:?}") };
    assert_eq!(path.len(), 32);
    assert_eq!(value, &serde_json::from_str::<Value>(&text[32 * 5..text.len() - 32]).unwrap());
}

#[test]
fn reports_nothing_for_values_in_invalid_positions_and_text_after_the_root() {
    let mut parser = StreamingJsonParser::new();
    let expected = [set("", json!({})), set("a", json!(1)), set("b", json!("x"))];
    assert_eq!(parser.append(r#"{"a":1 2, "b" "x"}"#), expected);
    assert_eq!(parser.append(r#"{"c":3}"#), []);
}

/// `write`-style arguments with `size` bytes of file content.
fn write_arguments(size: usize) -> (Value, String) {
    let mut content = String::new();
    for line in 0.. {
        if content.len() >= size {
            break;
        }
        content.push_str(&format!("const line{line} = \"value {line}\"; // \\ done\n"));
    }
    content.truncate(size);
    let arguments = json!({"path": "/src/file.ts", "content": content});
    let text = arguments.to_string();
    (arguments, text)
}

// #9265: re-parsing the accumulated text on every delta made a stream quadratic in its length.
#[test]
fn streams_a_megabyte_of_arguments_in_one_byte_deltas_in_linear_time() {
    let (arguments, text) = write_arguments(1 << 20);
    let started = Instant::now();
    let mut stream = ToolCallArgumentsStream::new();
    let mut shown = 0;
    for piece in pieces(&text, 1) {
        let partial = stream.append(piece);
        let content = partial.get("content").and_then(Value::as_str).unwrap_or_default();
        assert!(content.len() >= shown);
        shown = content.len();
    }
    assert_eq!(stream.finish(), (arguments.as_object().cloned().unwrap(), None));
    assert!(started.elapsed() < Duration::from_secs(30), "took {:?}", started.elapsed());
}

#[test]
fn reads_deep_nesting_without_recursion_and_in_linear_time() {
    let depth = 200_000;
    let text = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
    let started = Instant::now();
    let mut parser = StreamingJsonParser::new();
    for piece in pieces(&text, 7) {
        parser.append(piece);
        parser.snapshot();
    }
    // Containers deeper than serde_json's recursion limit are dropped.
    let mut value = parser.snapshot().unwrap();
    let mut levels = 0;
    while let Some(first) = value.as_array().and_then(|items| items.first()) {
        value = first;
        levels += 1;
    }
    assert_eq!((levels, value), (127, &json!([])));
    assert!(started.elapsed() < Duration::from_secs(30), "took {:?}", started.elapsed());
}

#[test]
fn keeps_the_preview_of_a_large_open_array_current() {
    let items: Vec<usize> = (0..100_000).collect();
    let text = json!({"items": items}).to_string();
    let mut parser = StreamingJsonParser::new();
    let mut shown = 0;
    for piece in pieces(&text, 16) {
        parser.append(piece);
        let count = parser.snapshot().and_then(|value| value["items"].as_array()).map_or(0, Vec::len);
        assert!(count >= shown);
        shown = count;
    }
    assert_eq!(parser.snapshot(), Some(&json!({"items": items})));
}

#[test]
fn shows_partial_arguments_as_an_object_and_finishes_with_one_full_parse() {
    let mut arguments = ToolCallArgumentsStream::new();
    assert_eq!(arguments.append("  "), &JsonObject::new());
    assert_eq!(
        arguments.append(r#"{"path":"a.ts","content":"x"#),
        json!({"path": "a.ts", "content": "x"}).as_object().unwrap()
    );
    arguments.append("y");
    assert_eq!(arguments.text(), r#"  {"path":"a.ts","content":"xy"#);
    assert_eq!(arguments.finish(), crate::json::parse_tool_call_arguments(r#"  {"path":"a.ts","content":"xy"#));
    arguments.append("\"}");
    assert_eq!(arguments.finish(), (json!({"path": "a.ts", "content": "xy"}).as_object().cloned().unwrap(), None));
    let mut arguments = ToolCallArgumentsStream::new();
    assert_eq!(arguments.append("[1,2"), &JsonObject::new());
    arguments.append("]");
    assert!(arguments.finish().1.unwrap().starts_with("Tool arguments must be a JSON object"));
}

#[test]
fn replaces_the_text_with_the_providers_complete_text_returning_the_missing_piece() {
    let mut arguments = ToolCallArgumentsStream::new();
    arguments.append(r#"{"a":"x"#);
    let (delta, partial) = arguments.replace(r#"{"a":"xyz"}"#);
    assert_eq!((delta.as_deref(), partial), (Some("yz\"}"), json!({"a": "xyz"}).as_object().unwrap()));
    let (delta, partial) = arguments.replace(r#"{"b":2}"#);
    assert_eq!((delta, partial), (None, json!({"b": 2}).as_object().unwrap()));
    assert_eq!(arguments.text(), r#"{"b":2}"#);
}

#[test]
fn agrees_with_parse_streaming_json_on_complete_malformed_strings() {
    let text = "{\"path\":\"p\",\"content\":\"a\tb\n\\d\\x41\\u12\\uZZZZ\\u00e9\\\\\\q\u{0}\",\"n\":1}";
    assert_eq!(final_snapshot(text, 1), parse_streaming_json(text));
}
