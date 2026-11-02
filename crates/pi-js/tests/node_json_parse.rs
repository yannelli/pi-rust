//! JSON.parse results and SyntaxError messages match real Node 22. `fixtures/json_parse_cases.cjs` holds the
//! cases; running it with Node writes `fixtures/json_parse_node.json`, and both backends must give the same cases.

mod common;
mod node_common;

use std::path::Path;

use common::*;
use serde_json::Value;

fn golden() -> Value {
    let golden: Value = serde_json::from_str(include_str!("fixtures/json_parse_node.json")).unwrap();
    assert!(golden.get("_status").is_none(), "{}", golden["_status"]);
    golden
}

async fn cases(runtime: &pi_js::JsRuntime, root: &Path) -> Value {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json_parse_cases.cjs");
    let path = serde_json::to_string(&fixture.display().to_string()).unwrap();
    run_cjs_within(runtime, root, &format!("return require({path})();"), 60).await
}

/// Every case that differs, so one run reports them all.
fn differences(actual: &Value, expected: &Value) -> Vec<String> {
    let (Some(actual), Some(expected)) = (actual.as_object(), expected.as_object()) else {
        return vec![format!("not objects: {actual} vs {expected}")];
    };
    let mut found: Vec<String> = expected
        .iter()
        .filter(|(name, value)| actual.get(*name) != Some(*value))
        .map(|(name, value)| format!("{name}: got {}, Node {value}", actual.get(name).unwrap_or(&Value::Null)))
        .collect();
    found.extend(actual.keys().filter(|name| !expected.contains_key(*name)).map(|name| format!("{name}: not in Node")));
    found
}

#[tokio::test]
async fn json_parse_matches_node_on_quickjs() {
    let golden = golden();
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let found = differences(&cases(&runtime, &root).await, &golden["cases"]);
    assert!(found.is_empty(), "QuickJS differs from Node {}:\n{}", golden["node"], found.join("\n"));
}

#[tokio::test]
async fn json_parse_matches_node_on_the_node_backend() {
    let golden = golden();
    let (_dir, root) = tempdir();
    let capture = std::sync::Arc::new(Capture::default());
    let Some(runtime) = node_common::node_runtime_with(options(&root, &capture)) else { return };
    let found = differences(&cases(&runtime, &root).await, &golden["cases"]);
    assert!(found.is_empty(), "the Node backend differs from Node {}:\n{}", golden["node"], found.join("\n"));
}
