//! node:test: TAP and spec output, hooks, mocks, run() with the reporters, snapshots, and prefix-only
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

// Snapshots through run(): missing files, --test-update-snapshots in execArgv, a passing rerun,
// mismatches, and a malformed snapshot file, as Node 22 reports them.
const SNAPSHOT_CASE: &str = r##"
const { run } = require("node:test");
const fs = require("fs");
const path = require("path");
const dir = path.resolve("snap");
fs.mkdirSync(dir, { recursive: true });
const file = path.join(dir, "s.test.js");
const source = (value) => `const test = require("node:test");
const path = require("path");
test("values", (t) => {
  t.assert.snapshot({ a: ${value}, list: ["x\`y", "\${z}", "back\\\\slash"] });
  t.assert.snapshot("second");
});
test.describe("group", () => {
  test.it("inner", (t) => t.assert.snapshot([1, 2], { serializers: [(v) => v.join("+")] }));
});
test("file", (t) => t.assert.fileSnapshot({ b: ${value} }, path.join(__dirname, "out", "file.json")));
test("bad", (t) => t.assert.snapshot(1, { serializers: "no" }));
`;
async function results(execArgv) {
  const events = [];
  for await (const event of run({ files: [file], execArgv })) {
    if (event.type === "test:pass" || event.type === "test:fail") {
      const error = event.data.details.error;
      events.push([event.type, event.data.name, error ? [error.cause?.code, String(error.cause?.message).split("\n")[0].replaceAll(dir, "<dir>").replaceAll("\\", "/")] : null]);
    }
  }
  return events;
}
const out = {};
fs.writeFileSync(file, source(1));
out.missing = await results([]);
out.update = await results(["--test-update-snapshots"]);
out.snapshotFile = fs.readFileSync(file + ".snapshot", "utf8");
out.fileSnapshot = fs.readFileSync(path.join(dir, "out", "file.json"), "utf8");
out.pass = await results([]);
fs.writeFileSync(file, source(2));
out.mismatch = await results([]);
fs.writeFileSync(file + ".snapshot", "exports = 5;");
out.malformed = await results([]);
return out;
"##;

const SNAPSHOT_EXPECTED: &str = r##"{"missing":[["test:fail","values",["ERR_INVALID_STATE","Invalid state: Cannot read snapshot file '<dir>/s.test.js.snapshot.' Missing snapshots can be generated by rerunning the command with the --test-update-snapshots flag."]],["test:fail","inner",["ERR_INVALID_STATE","Invalid state: Cannot read snapshot file '<dir>/s.test.js.snapshot.' Missing snapshots can be generated by rerunning the command with the --test-update-snapshots flag."]],["test:fail","group",[null,"undefined"]],["test:fail","file",["ERR_INVALID_STATE","Invalid state: Cannot read snapshot file '<dir>/out/file.json.' Missing snapshots can be generated by rerunning the command with the --test-update-snapshots flag."]],["test:fail","bad",["ERR_INVALID_ARG_TYPE","The \"options.serializers\" property must be an instance of Array. Received type string ('no')"]]],"update":[["test:pass","values",null],["test:pass","inner",null],["test:pass","group",null],["test:pass","file",null],["test:fail","bad",["ERR_INVALID_ARG_TYPE","The \"options.serializers\" property must be an instance of Array. Received type string ('no')"]]],"snapshotFile":"exports[`group > inner 1`] = `\n1+2\n`;\n\nexports[`values 1`] = `\n{\n  \"a\": 1,\n  \"list\": [\n    \"x\\`y\",\n    \"\\${z}\",\n    \"back\\\\\\\\slash\"\n  ]\n}\n`;\n\nexports[`values 2`] = `\n\"second\"\n`;\n","fileSnapshot":"{\n  \"b\": 1\n}","pass":[["test:pass","values",null],["test:pass","inner",null],["test:pass","group",null],["test:pass","file",null],["test:fail","bad",["ERR_INVALID_ARG_TYPE","The \"options.serializers\" property must be an instance of Array. Received type string ('no')"]]],"mismatch":[["test:fail","values",["ERR_ASSERTION","Expected values to be strictly equal:"]],["test:pass","inner",null],["test:pass","group",null],["test:fail","file",["ERR_ASSERTION","Expected values to be strictly equal:"]],["test:fail","bad",["ERR_INVALID_ARG_TYPE","The \"options.serializers\" property must be an instance of Array. Received type string ('no')"]]],"malformed":[["test:fail","values",["ERR_INVALID_STATE","Invalid state: Cannot read snapshot file '<dir>/s.test.js.snapshot.'"]],["test:fail","inner",["ERR_INVALID_STATE","Invalid state: Cannot read snapshot file '<dir>/s.test.js.snapshot.'"]],["test:fail","group",[null,"undefined"]],["test:fail","file",["ERR_ASSERTION","Expected values to be strictly equal:"]],["test:fail","bad",["ERR_INVALID_ARG_TYPE","The \"options.serializers\" property must be an instance of Array. Received type string ('no')"]]]}"##;

#[tokio::test]
async fn snapshot_assertions_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, SNAPSHOT_CASE, 60).await;
    assert_same_keys(&actual, &serde_json::from_str(SNAPSHOT_EXPECTED).unwrap());
}
