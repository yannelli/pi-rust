//! `loadProjectContextFiles` of `@earendil-works/pi-coding-agent` is upstream code: context file order and precedence,
//! BOM and UTF-8 handling, read warnings, and linked-worktree shadowing, with chalk's yellow from a private host port.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use std::collections::BTreeMap;
use std::io::IsTerminal;

use harness::{Setup, harness};
use serde_json::Value;

/// Upstream output, written by `node scripts/project-context-scenario.mjs <upstream>`.
const EXPECTED: &str = include_str!("fixtures/extensions/project_context_scenario.json");
/// Upstream chalk 6.0.0 output per environment, written by the same script.
const COLOR_CASES: &str = include_str!("fixtures/extensions/context_color_cases.json");
const COLOR_PORT: &str = include_str!("../src/extensions/js/context_color.js");
/// The one case the scenario may skip, where permission bits cannot make a file unreadable (Windows or root).
const PERMISSION_CASE: (&str, &str) =
    ("an unreadable AGENTS.md warns and falls through", "skipped: no permission bits");

const EXTENSION: &str = r#"
import { loadProjectContextFiles } from '@earendil-works/pi-coding-agent';
import { scenario } from './project_context_scenario.mjs';

export default function (pi) {
  pi.registerCommand('context', { handler: async () => {
    const data = await scenario({ loadProjectContextFiles });
    let chalk = 'absent';
    try {
      await import('chalk');
      chalk = 'present';
    } catch {}
    pi.appendEntry('context', { data, chalk, name: loadProjectContextFiles.name });
  }});
}
"#;

fn assert_generated(expected: &Value, file: &str) {
    if let Some(status) = expected.get("_status") {
        panic!("{file} holds expectations derived from source, not upstream output: {status}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_project_context_files_matches_upstream() {
    let files = vec![
        ("context.ts", EXTENSION.to_string()),
        ("project_context_scenario.mjs", include_str!("fixtures/extensions/project_context_scenario.mjs").to_string()),
    ];
    let h = harness(Setup { files, load: vec!["context.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/context", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(30), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "context").expect("context entry");
    assert_eq!(entry["data"]["name"], "loadProjectContextFiles");
    assert_eq!(entry["data"]["chalk"], "absent", "the chalk port must stay private to the vendored loader");
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    let cases: BTreeMap<&String, &Value> =
        expected.as_object().unwrap().iter().filter(|(name, _)| *name != "_status").collect();
    let actual = entry["data"]["data"].as_object().unwrap();
    assert_eq!(actual.len(), cases.len(), "{:?}", actual.keys().collect::<Vec<_>>());
    let (mut compared, mut skipped) = (0, 0);
    for (name, case) in &cases {
        let value = &actual[name.as_str()];
        if (name.as_str(), value.as_str()) == (PERMISSION_CASE.0, Some(PERMISSION_CASE.1)) {
            skipped += 1;
            continue;
        }
        assert!(!value.as_str().is_some_and(|text| text.starts_with("skipped:")), "{name}: unexpected skip {value}");
        assert_eq!(value, *case, "{name}");
        compared += 1;
    }
    println!("project context scenario: {compared} compared, {skipped} skipped");
    assert!(skipped <= 1 && compared + skipped == cases.len(), "{compared} compared, {skipped} skipped");
    assert_generated(&expected, "project_context_scenario.json");
}

/// Each case loads the port in a fresh runtime with only the case's environment and arguments, as the level is fixed
/// at load. `nonTerminalOnly` cases need a piped stdout; on Windows a case's `win32Outputs` apply when present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_private_yellow_port_matches_upstream_chalk() {
    let doc: Value = serde_json::from_str(COLOR_CASES).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("context_color.mjs"), COLOR_PORT).unwrap();
    let probes = serde_json::to_string(&doc["probes"]).unwrap();
    let probe = format!(
        "import color from './context_color.mjs';\nexport const outputs = {probes}.map((text) => color.yellow(text));\n"
    );
    let terminal = std::io::stdout().is_terminal();
    let cases = doc["cases"].as_array().unwrap();
    let (mut ran, mut gated) = (0, 0);
    for case in cases {
        if case["nonTerminalOnly"] == true && terminal {
            gated += 1;
            continue;
        }
        let mut options = pi_js::JsRuntimeOptions::new(dir.path());
        options.tokio = Some(tokio::runtime::Handle::current());
        options.env = Some(serde_json::from_value(case["env"].clone()).unwrap());
        let argv: Vec<String> = serde_json::from_value(case["argv"].clone()).unwrap();
        options.argv = ["pi".to_string(), "probe.mjs".to_string()].into_iter().chain(argv).collect();
        let runtime = pi_js::JsRuntime::new(options).unwrap();
        let module = runtime.eval_module("probe.mjs", probe.clone()).await.unwrap();
        let outputs = module.get_ref("outputs").await.unwrap().to_json().await.unwrap();
        let windows = cfg!(windows) && case.get("win32Outputs").is_some();
        let expected = if windows { &case["win32Outputs"] } else { &case["outputs"] };
        assert!(expected.is_array(), "{}: expected outputs not generated", case["name"]);
        assert_eq!(&outputs, expected, "{}", case["name"]);
        ran += 1;
    }
    let platform = if cfg!(windows) { "win32" } else { "non-win32" };
    println!("context color cases ({platform}): {ran} ran, {gated} skipped as stdout is a terminal");
    let expected_gated = cases.iter().filter(|case| case["nonTerminalOnly"] == true).count();
    assert_eq!(ran + gated, cases.len());
    assert_eq!(gated, if terminal { expected_gated } else { 0 });
    assert_generated(&doc, "context_color_cases.json");
}
