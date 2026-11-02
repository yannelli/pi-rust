//! The 609 oracle cases of upstream `SettingsManager` (earendil-works/pi 35dfa757), replayed against
//! pi-rust's SDK `SettingsManager` and compared with the golden the authentic upstream class wrote.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use serde::Serialize;
use serde_json::Value;

/// sha256 of the oracle's `golden.json`, and of the compact copy kept here.
const GOLDEN_SHA256: &str = "08c65fea1538fc3ee7b758cabf1eff27800aba5601f1a9f86e701e93819ac0ab";
const COMPACT_SHA256: &str = "dc37cda47a080a4ebe66af708befb1606cafaa048f8bd26b4400a4488b083a7d";
const GOLDEN: &str = include_str!("fixtures/sdk_settings/golden.json");

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn golden() -> Value {
    serde_json::from_str(GOLDEN).expect("golden.json parses")
}

#[test]
fn the_compact_golden_is_the_oracle_output() {
    assert_eq!(sha256(GOLDEN.as_bytes()), COMPACT_SHA256);
    let value = golden();
    let mut expanded = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut expanded, formatter);
    value.serialize(&mut serializer).unwrap();
    expanded.push(b'\n');
    assert_eq!(sha256(&expanded), GOLDEN_SHA256, "re-expanded with indent 1 it is byte-identical to the oracle file");
    assert_eq!(value["cases"].as_array().map(Vec::len), Some(609));
    assert_eq!(value["memberCount"], 139);
}

/// Compares values, key order included.
fn compare(path: &str, golden: &Value, ours: &Value, mismatches: &mut Vec<String>) {
    match (golden, ours) {
        (Value::Object(expected), Value::Object(actual)) => {
            let (keys, actual_keys): (Vec<_>, Vec<_>) = (expected.keys().collect(), actual.keys().collect());
            if keys != actual_keys {
                mismatches.push(format!("{path}: keys {keys:?}, got {actual_keys:?}"));
            }
            for (key, value) in expected {
                if let Some(other) = actual.get(key) {
                    compare(&format!("{path}/{key}"), value, other, mismatches);
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                mismatches.push(format!("{path}: {} items, got {}", expected.len(), actual.len()));
            }
            for (index, (value, other)) in expected.iter().zip(actual).enumerate() {
                compare(&format!("{path}/{index}"), value, other, mismatches);
            }
        }
        _ if golden != ours => mismatches.push(format!("{path}: expected {golden}, got {ours}")),
        _ => {}
    }
}

// Upstream emits native Windows paths and notepad. Its JSON-text replacement leaves paths unnormalized.
// Adapt only the golden; normalizing actual paths would hide incorrect separators.
#[cfg(windows)]
fn on_this_platform(mut golden: Value, run: &std::path::Path) -> Value {
    fn expand(value: &mut Value, run: &str, home: &str) -> usize {
        match value {
            Value::String(text) => {
                let at_run = text.strip_prefix("<RUN>/").map(|rest| (run, rest));
                let Some((folder, rest)) = at_run.or_else(|| text.strip_prefix("<HOME>/").map(|rest| (home, rest)))
                else {
                    assert!(!text.contains("<RUN>") && !text.contains("<HOME>"), "{text}");
                    return 0;
                };
                *text = format!("{folder}\\{}", rest.replace('/', "\\"));
                1
            }
            Value::Array(items) => items.iter_mut().map(|item| expand(item, run, home)).sum(),
            Value::Object(map) => map.values_mut().map(|item| expand(item, run, home)).sum(),
            _ => 0,
        }
    }
    let (run, home) = (run.display().to_string(), std::env::var("USERPROFILE").expect("USERPROFILE"));
    let editor_fallbacks = ["getter/getExternalEditorCommand/empty", "getter/getExternalEditorCommand/invalid"];
    let mut counts = (0, 0);
    for case in golden["cases"].as_array_mut().unwrap() {
        if editor_fallbacks.contains(&case["id"].as_str().unwrap_or_default()) {
            let returned = &mut case["steps"][0]["returned"];
            assert_eq!(*returned, "nano");
            *returned = Value::from("notepad");
            counts.1 += 1;
        }
        counts.0 += expand(case, &run, &home);
    }
    assert_eq!(counts, (10, 2), "folder values and editor fallbacks rewritten");
    golden
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_oracle_cases_replay_against_the_sdk_settings_manager() {
    let fixture = |path: &'static str, source: &str| (path, source.to_string());
    let files = vec![
        fixture("replay.ts", include_str!("fixtures/sdk_settings/replay.ts")),
        fixture("oracle/harness.mjs", include_str!("fixtures/sdk_settings/oracle/harness.mjs")),
        fixture("data/fixtures.mjs", include_str!("fixtures/sdk_settings/data/fixtures.mjs")),
        fixture("data/members.mjs", include_str!("fixtures/sdk_settings/data/members.mjs")),
        fixture("data/lifecycle.mjs", include_str!("fixtures/sdk_settings/data/lifecycle.mjs")),
        fixture("data/file-cases.mjs", include_str!("fixtures/sdk_settings/data/file-cases.mjs")),
    ];
    let h = harness(Setup { files, load: vec!["replay.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(240), h.session.prompt("/replay", Vec::new()))
        .await
        .expect("the replay finishes")
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let replay: Value = serde_json::from_str(&std::fs::read_to_string(h.cwd.join("replay.json")).unwrap()).unwrap();
    let golden = golden();
    #[cfg(windows)]
    let golden = on_this_platform(golden, &h.cwd.join("oracle-run"));
    let (expected, actual) = (golden["cases"].as_array().unwrap(), replay["cases"].as_array().unwrap());
    let ids = |cases: &[Value]| {
        cases.iter().map(|case| case["id"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>()
    };
    assert_eq!(ids(actual.as_slice()), ids(expected.as_slice()), "the same 609 cases in the same order");

    let mut mismatches = Vec::new();
    for (case, ours) in expected.iter().zip(actual) {
        compare(case["id"].as_str().unwrap_or_default(), case, ours, &mut mismatches);
    }
    let shown: Vec<_> = mismatches.iter().take(40).collect();
    assert!(mismatches.is_empty(), "{} differences from the golden:\n{shown:#?}", mismatches.len());

    let used = replay["used"].as_object().unwrap();
    let members = golden["memberCount"].as_u64().unwrap() as usize;
    assert_eq!(used.len(), members, "every public member ran: {:?}", used.keys().collect::<Vec<_>>());
    assert!(used.values().all(|count| count.as_u64() >= Some(2)), "each member ran in at least 2 cases: {used:?}");
}

#[cfg(windows)]
#[tokio::test]
async fn directory_settings_error_matches_real_node() {
    use pi_coding_agent::settings::{FileSettingsStorage, SettingsScope, SettingsStorage};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("settings.json");
    std::fs::create_dir(&path).unwrap();
    let node = std::env::var_os("PI_NODE_PATH").unwrap_or_else(|| "node".into());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(node)
            .args(["-e", "try { require('node:fs').readFileSync(process.argv[1], 'utf8'); } catch (e) { process.stdout.write(e.message); }"])
            .arg(&path)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the Node directory control finishes")
    .expect("Node must run the Windows directory control");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let expected = String::from_utf8(output.stdout).unwrap();
    assert_eq!(expected, "EISDIR: illegal operation on a directory, read");
    let storage = FileSettingsStorage::new(root.path(), root.path());
    let actual = storage.with_lock(SettingsScope::Global, &mut |_| panic!("a directory must not be read"));
    assert_eq!(actual.unwrap_err(), expected);
}
