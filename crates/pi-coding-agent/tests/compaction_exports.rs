//! The compaction exports of `@earendil-works/pi-coding-agent` are upstream code: prompts, request
//! options, retries, errors, split-turn usage, branch preparation, and the default `completeSimple` path.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use serde_json::Value;

/// Upstream output of the scenario, written by `node scripts/compaction-scenario.mjs <upstream>`.
const EXPECTED: &str = include_str!("fixtures/extensions/compaction_scenario.json");

const EXTENSION: &str = r#"
import { compact, generateBranchSummary, generateSummary, generateSummaryWithUsage } from '@earendil-works/pi-coding-agent';
import { fauxAssistantMessage, registerFauxProvider } from '@earendil-works/pi-ai';
import { scenario } from './compaction_scenario.mjs';

export default function (pi) {
  pi.registerCommand('compaction', { handler: async () => {
    const exports = { compact, generateBranchSummary, generateSummary, generateSummaryWithUsage, fauxAssistantMessage, registerFauxProvider };
    pi.appendEntry('compaction', await scenario(exports));
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_exports_match_upstream() {
    let files = vec![
        ("compaction.ts", EXTENSION.to_string()),
        ("compaction_scenario.mjs", include_str!("fixtures/extensions/compaction_scenario.mjs").to_string()),
    ];
    let h = harness(Setup { files, load: vec!["compaction.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/compaction", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "compaction").expect("compaction entry");
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    let cases = expected.as_object().unwrap();
    assert_eq!(entry["data"].as_object().unwrap().len(), cases.len());
    for (name, case) in cases {
        assert_eq!(&entry["data"][name], case, "{name}");
    }
}
