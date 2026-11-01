//! The exported JavaScript `TuiAltScreen` gives fullscreen viewport keys to a focused component that captures them:
//! a SelectList, a component whose capture flips, the wheel, and an Editor while its autocomplete is open.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use serde_json::Value;

/// Upstream output, written by `node --experimental-strip-types scripts/tui-viewport-scenario.mjs <upstream>`.
const EXPECTED: &str = include_str!("fixtures/extensions/tui_viewport_scenario.json");

const EXTENSION: &str = r#"
import * as tui from '@earendil-works/pi-tui';
import { scenario } from './tui_viewport_scenario.mjs';

export default function (pi) {
  pi.registerCommand('viewport', { handler: async () => {
    pi.appendEntry('viewport', await scenario(tui));
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exported_tui_alt_screen_defers_viewport_keys_to_capturing_components() {
    let files = vec![
        ("viewport.ts", EXTENSION.to_string()),
        ("tui_viewport_scenario.mjs", include_str!("fixtures/extensions/tui_viewport_scenario.mjs").to_string()),
    ];
    let h = harness(Setup { files, load: vec!["viewport.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/viewport", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "viewport").expect("viewport entry");
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    let cases = expected.as_object().unwrap();
    assert_eq!(entry["data"].as_object().unwrap().len(), cases.len());
    for (name, case) in cases {
        assert_eq!(&entry["data"][name], case, "{name}");
    }
}
