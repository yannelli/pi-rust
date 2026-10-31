//! pi-tui's `Markdown` in an extension is upstream code: it renders the cases of five upstream changes as
//! upstream does under Node. CI runs this on QuickJS and, with `PI_EXTENSION_RUNTIME=node`, on Node.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use serde_json::Value;

/// Upstream output, written by `node scripts/markdown-scenario.mjs <upstream>`.
const EXPECTED: &str = include_str!("fixtures/extensions/markdown_scenario.json");

const EXTENSION: &str = r#"
import * as tui from '@earendil-works/pi-tui';
import { scenario } from './markdown_scenario.mjs';

export default function (pi) {
  pi.registerCommand('markdown', { handler: async () => {
    pi.appendEntry('markdown', scenario(tui));
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_renders_the_upstream_cjk_math_table_and_quote_cases() {
    let files = vec![
        ("markdown.ts", EXTENSION.to_string()),
        ("markdown_scenario.mjs", include_str!("fixtures/extensions/markdown_scenario.mjs").to_string()),
    ];
    let h = harness(Setup { files, load: vec!["markdown.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/markdown", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "markdown").expect("markdown entry");
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    let cases = expected["rendered"].as_object().unwrap();
    assert_eq!(cases.len(), 16);
    for (name, rendered) in cases {
        assert_eq!(&entry["data"]["rendered"][name], rendered, "{name}");
    }
}
