//! The standalone interactive components of `@earendil-works/pi-coding-agent` are upstream code:
//! extensions build, render, resize, and drive them as upstream does under Node.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use serde_json::Value;

/// Upstream output of the scenario, written by `node scripts/component-scenario.mjs <upstream>`.
const EXPECTED: &str = include_str!("fixtures/extensions/component_scenario.json");

const EXTENSION: &str = r#"
import * as ca from '@earendil-works/pi-coding-agent';
import * as legacy from '@mariozechner/pi-coding-agent';
import { scenario } from './component_scenario.mjs';

const NAMES = ['ArminComponent', 'AssistantMessageComponent', 'BashExecutionComponent', 'BranchSummaryMessageComponent',
  'CompactionSummaryMessageComponent', 'CustomMessageComponent', 'ExtensionInputComponent', 'ExtensionSelectorComponent',
  'OAuthSelectorComponent', 'ShowImagesSelectorComponent', 'SkillInvocationMessageComponent', 'ThinkingSelectorComponent',
  'UserMessageComponent', 'UserMessageSelectorComponent'];

export default function (pi) {
  pi.registerCommand('components', { handler: async () => {
    const shared = NAMES.filter((name) => typeof ca[name] !== 'function' || ca[name] !== legacy[name]);
    pi.appendEntry('components', { scenario: scenario(ca), shared });
  }});
}
"#;

/// The `components` entry an extension in `files` appends for `/components`.
async fn components_entry(files: Vec<(&'static str, String)>) -> Value {
    let h = harness(Setup { files, load: vec!["components.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/components", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "components").expect("components entry");
    entry["data"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_components_render_and_handle_input_as_upstream() {
    let files = vec![
        ("components.ts", EXTENSION.to_string()),
        ("component_scenario.mjs", include_str!("fixtures/extensions/component_scenario.mjs").to_string()),
    ];
    let data = &components_entry(files).await;
    assert_eq!(data["shared"], Value::Array(Vec::new()), "{data}");
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    assert_eq!(data["scenario"]["events"], expected["events"]);
    assert_eq!(data["scenario"]["armin"], expected["armin"]);
    for (name, rendered) in expected["rendered"].as_object().unwrap() {
        assert_eq!(&data["scenario"]["rendered"][name], rendered, "{name}");
    }
}

const EXTENSION_B: &str = r#"
import * as ca from '@earendil-works/pi-coding-agent';
import { getKeybindings } from '@earendil-works/pi-tui';
import { scenario } from './component_scenario_b.mjs';

export default function (pi) {
  pi.registerCommand('components', { handler: async () => {
    pi.appendEntry('components', { scenario: await scenario({ ...ca, getKeybindings }), shared: [] });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_driven_selectors_and_the_login_dialog_match_upstream() {
    let files = vec![
        ("components.ts", EXTENSION_B.to_string()),
        ("component_scenario_b.mjs", include_str!("fixtures/extensions/component_scenario_b.mjs").to_string()),
    ];
    let expected: Value = serde_json::from_str(include_str!("fixtures/extensions/component_scenario_b.json")).unwrap();
    let data = components_entry(files).await;
    assert_eq!(data["scenario"]["events"], expected["events"]);
    for (name, rendered) in expected["rendered"].as_object().unwrap() {
        assert_eq!(&data["scenario"]["rendered"][name], rendered, "{name}");
    }
}
