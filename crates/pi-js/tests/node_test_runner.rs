//! node:test: TAP and spec output, hooks, mocks, run() with the reporters, and prefix-only
//! loading. The scripts are in fixtures/node_test; the expected output there comes from Node 22,
//! with durations, stack frames, and source locations removed on both sides.

mod common;

use common::*;

/// Drop stack blocks, frames, and locations, and replace durations, as the expectations do.
fn normalize(text: &str) -> String {
    let mut out = Vec::new();
    let mut skip = None;
    for line in text.split('\n') {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if let Some(limit) = skip {
            if indent > limit && !line.trim().is_empty() {
                continue;
            }
            skip = None;
        }
        let trimmed = line.trim_start_matches(' ');
        if trimmed == "stack: |-" {
            skip = Some(indent);
            continue;
        }
        if ["location: '", "at ", "test at "].iter().any(|prefix| trimmed.starts_with(prefix)) {
            continue;
        }
        out.push(replace_numbers(line));
    }
    out.join("\n")
}

fn replace_numbers(line: &str) -> String {
    let mut line = line.to_string();
    for (marker, end) in [("duration_ms: ", ""), ("duration_ms ", ""), ("(", "ms)")] {
        let mut from = 0;
        while let Some(found) = line[from..].find(marker) {
            let start = from + found + marker.len();
            let digits = line[start..]
                .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == 'e' || c == '-'))
                .map_or(line.len(), |n| start + n);
            if digits > start && line[digits..].starts_with(end) {
                line.replace_range(start..digits, "X");
            }
            from = start;
        }
    }
    line
}

async fn run_file(name: &str, source: &str) -> (String, serde_json::Value, tempfile::TempDir) {
    let (dir, root) = tempdir();
    write(&root, name, source);
    let (runtime, capture) = runtime(&root);
    runtime.import(root.join(name).display().to_string()).await.unwrap();
    runtime.idle().await.unwrap();
    let state = runtime.eval("JSON.stringify({ exitCode: process.exitCode, order: globalThis.order })").await.unwrap();
    let state: serde_json::Value = serde_json::from_str(state.into_json().as_str().unwrap()).unwrap();
    (capture.stdout(), state, dir)
}

const TAP_CASE: &str = include_str!("fixtures/node_test/tap_case.js");

const TAP_EXPECTED: &str = include_str!("fixtures/node_test/tap.txt");

#[tokio::test]
async fn tap_output_matches_node() {
    let (stdout, state, _dir) = run_file("tap_case.js", TAP_CASE).await;
    assert_eq!(normalize(&stdout), TAP_EXPECTED);
    assert_eq!(state["exitCode"], 1);
}

const HOOKS_CASE: &str = include_str!("fixtures/node_test/hooks_case.js");

const HOOKS_ORDER: &str = include_str!("fixtures/node_test/hooks_order.json");

const HOOKS_TAP: &str = include_str!("fixtures/node_test/hooks_tap.txt");

#[tokio::test]
async fn hooks_run_in_node_order() {
    let (stdout, state, _dir) = run_file("hooks_case.js", HOOKS_CASE).await;
    let expected: serde_json::Value = serde_json::from_str(HOOKS_ORDER).unwrap();
    assert_eq!(state["order"], expected);
    assert_eq!(normalize(&stdout), HOOKS_TAP);
    assert_eq!(state["exitCode"], serde_json::Value::Null);
}

const MOCK_CASE: &str = include_str!("fixtures/node_test/mock_case.js");

const MOCK_EXPECTED: &str = include_str!("fixtures/node_test/mock.txt");

#[tokio::test]
async fn mocks_and_context() {
    let (stdout, _state, _dir) = run_file("mock_case.js", MOCK_CASE).await;
    assert_eq!(normalize(&stdout), MOCK_EXPECTED);
}

const SPEC_CASE: &str = include_str!("fixtures/node_test/spec_case.js");

const SPEC_EXPECTED: &str = include_str!("fixtures/node_test/spec.txt");

#[tokio::test]
async fn spec_reporter() {
    let (stdout, _state, _dir) = run_file("spec_case.js", SPEC_CASE).await;
    assert_eq!(normalize(&stdout), SPEC_EXPECTED);
}

const RUN_A: &str = include_str!("fixtures/node_test/runfiles/a.test.js");

const RUN_B: &str = include_str!("fixtures/node_test/runfiles/b.test.cjs");

const RUN_CASE: &str = include_str!("fixtures/node_test/run_case.js");

const RUN_EXPECTED: &str = include_str!("fixtures/node_test/run.json");

#[tokio::test]
async fn run_streams_events_to_reporters() {
    let (_dir, root) = tempdir();
    write(&root, "runfiles/a.test.js", RUN_A);
    write(&root, "runfiles/b.test.cjs", RUN_B);
    write(&root, "run_case.js", RUN_CASE);
    let (runtime, capture) = runtime(&root);
    runtime.import(root.join("run_case.js").display().to_string()).await.unwrap();
    runtime.idle().await.unwrap();
    let mut actual: serde_json::Value = serde_json::from_str(&capture.stdout()).unwrap();
    for key in ["tap", "dot", "spec"] {
        actual[key] = normalize(actual[key].as_str().unwrap()).into();
    }
    let expected: serde_json::Value = serde_json::from_str(RUN_EXPECTED).unwrap();
    assert_same_keys(&actual, &expected);
}

#[tokio::test]
async fn test_modules_load_only_with_the_prefix() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, r#"
const m = require("module");
const out = {};
for (const name of ["test", "test/reporters"]) {
  try { require(name); out[name] = "loaded"; } catch (e) { out[name] = e.code; }
}
out.isBuiltin = ["node:test", "node:test/reporters", "test"].map((name) => m.isBuiltin(name));
const t = require("node:test");
out.api = [t === t.test, t.it === t.test, t.suite === t.describe, typeof t.mock.fn, Object.keys(require("node:test/reporters")).sort()];
out.esm = typeof (await import("node:test")).default.describe;
return out;
"#).await;
    let expected = serde_json::json!({
        "test": "MODULE_NOT_FOUND",
        "test/reporters": "MODULE_NOT_FOUND",
        "isBuiltin": [true, true, false],
        "api": [true, true, true, "function", ["dot", "junit", "lcov", "spec", "tap"]],
        "esm": "function"
    });
    assert_eq!(actual, expected);
}
