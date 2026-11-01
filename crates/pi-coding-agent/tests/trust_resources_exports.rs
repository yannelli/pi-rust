//! `hasTrustRequiringProjectResources` of `@earendil-works/pi-coding-agent` is upstream code: `.pi`
//! entries, `.agents/skills` in the project or an ancestor, and the user's own skills left out.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use serde_json::Value;

/// Upstream output of the scenario, written by `node scripts/trust-resources-scenario.mjs <upstream>`.
const EXPECTED: &str = include_str!("fixtures/extensions/trust_resources_scenario.json");

const EXTENSION: &str = r#"
import { hasTrustRequiringProjectResources } from '@earendil-works/pi-coding-agent';
import { scenario } from './trust_resources_scenario.mjs';

export default function (pi) {
  pi.registerCommand('trust', { handler: async () => {
    const data = await scenario({ hasTrustRequiringProjectResources });
    pi.appendEntry('trust', { data, name: hasTrustRequiringProjectResources.name });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn has_trust_requiring_project_resources_matches_upstream() {
    let files = vec![
        ("trust.ts", EXTENSION.to_string()),
        ("trust_resources_scenario.mjs", include_str!("fixtures/extensions/trust_resources_scenario.mjs").to_string()),
    ];
    let h = harness(Setup { files, load: vec!["trust.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/trust", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "trust").expect("trust entry");
    assert_eq!(entry["data"]["name"], "hasTrustRequiringProjectResources");
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    let cases = expected.as_object().unwrap();
    assert_eq!(entry["data"]["data"].as_object().unwrap().len(), cases.len());
    for (name, case) in cases {
        assert_eq!(&entry["data"]["data"][name], case, "{name}");
    }
}
