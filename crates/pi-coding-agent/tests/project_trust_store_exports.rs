//! `ProjectTrustStore` of `@earendil-works/pi-coding-agent` is upstream code: reads, writes, inheritance, removal,
//! normalization, key order, store errors, and the lock protocol, checked against goldens from upstream on Node 22.
#![cfg(unix)]

mod cli_support;
#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use harness::{Setup, harness};
use serde_json::{Map, Value, json};

/// Upstream output, written by `node scripts/project-trust-store-scenario.mjs <upstream> --temp-parent <dir>`.
const SCENARIO: &str = include_str!("fixtures/extensions/project_trust_store_scenario.json");
/// The same script's child cases per mode: exit code, the child's report, markers, and the lock afterwards.
const EXIT: &str = include_str!("fixtures/extensions/project_trust_store_exit.json");
const SCENARIO_MODULE: &str = include_str!("fixtures/extensions/project_trust_store_scenario.mjs");
const EXIT_MODULE: &str = include_str!("fixtures/extensions/project_trust_store_exit.mjs");
/// Compared only in `the_json_parse_error_text_is_v8s`: the engine's own `JSON.parse` message.
const ENGINE_CASE: &str = "engine JSON.parse text";

const EXTENSION: &str = r#"
import { ProjectTrustStore } from '@earendil-works/pi-coding-agent';
import { scenario } from './project_trust_store_scenario.mjs';

export default function (pi) {
  pi.registerCommand('truststore', { handler: async () => {
    const data = await scenario({ ProjectTrustStore }, { tempParent: __TEMP_PARENT__ });
    let lockfile = 'absent';
    try {
      await import('proper-lockfile');
      lockfile = 'present';
    } catch {}
    pi.appendEntry('truststore', { data, lockfile, name: ProjectTrustStore.name });
  }});
}
"#;

const EXIT_EXTENSION: &str = r#"
import { ProjectTrustStore } from '@earendil-works/pi-coding-agent';
import { run } from './project_trust_store_exit.mjs';

export default function (pi) {
  pi.registerCommand('trustexit', { handler: async () => {
    await run({ ProjectTrustStore }, __OPTIONS__);
  }});
}
"#;

fn golden(text: &str, file: &str) -> Map<String, Value> {
    let value: Value = serde_json::from_str(text).unwrap();
    if let Some(status) = value.get("_status") {
        panic!("{file} holds no upstream output yet: {status}");
    }
    let mut map = value.as_object().unwrap().clone();
    let generator = map.shift_remove("_generator").unwrap_or_else(|| panic!("{file} has no _generator record"));
    assert!(generator["node"].as_str().is_some_and(|node| node.starts_with("v22.")), "{file}: {generator}");
    map
}

/// `value` without its `engine` fields, which hold the engine's own `JSON.parse` text.
fn without_engine(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter().filter(|(key, _)| *key != "engine").map(|(k, v)| (k.clone(), without_engine(v))).collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_engine).collect()),
        other => other.clone(),
    }
}

fn engine_fields(name: &str, value: &Value, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                if key == "engine" {
                    out.push((name.to_string(), inner.clone()));
                } else {
                    engine_fields(&format!("{name}/{key}"), inner, out);
                }
            }
        }
        Value::Array(items) => {
            items.iter().enumerate().for_each(|(i, item)| engine_fields(&format!("{name}/{i}"), item, out))
        }
        _ => {}
    }
}

/// Runs the scenario in the extension host and returns its cases.
async fn run_scenario() -> Map<String, Value> {
    let parent = tempfile::tempdir().unwrap();
    let parent_path = std::fs::canonicalize(parent.path()).unwrap();
    let parent_json = serde_json::to_string(&parent_path.display().to_string()).unwrap();
    let files = vec![
        ("truststore.ts", EXTENSION.replace("__TEMP_PARENT__", &parent_json)),
        ("project_trust_store_scenario.mjs", SCENARIO_MODULE.to_string()),
    ];
    let h = harness(Setup { files, load: vec!["truststore.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/truststore", Vec::new());
    tokio::time::timeout(Duration::from_secs(60), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "truststore").expect("truststore entry");
    assert_eq!(entry["data"]["name"], "ProjectTrustStore");
    assert_eq!(entry["data"]["lockfile"], "absent", "the lock port must stay private to the vendored trust manager");
    entry["data"]["data"].as_object().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_trust_store_matches_upstream() {
    let expected = golden(SCENARIO, "project_trust_store_scenario.json");
    let actual = run_scenario().await;
    assert_eq!(actual.keys().collect::<Vec<_>>(), expected.keys().collect::<Vec<_>>());
    let mut compared = 0;
    for (name, case) in &expected {
        if name == ENGINE_CASE {
            continue;
        }
        assert_eq!(without_engine(&actual[name]), without_engine(case), "{name}");
        compared += 1;
    }
    println!("project trust store scenario: {compared} compared, {ENGINE_CASE:?} left to the JSON.parse test");
    assert_eq!(compared, expected.len() - 1);
}

/// `readTrustFile` puts the engine's `JSON.parse` message in its error. Upstream's is V8's; this checks the host
/// gives the same text, so a QuickJS difference shows here as its own failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_json_parse_error_text_is_v8s() {
    let expected = golden(SCENARIO, "project_trust_store_scenario.json");
    let actual = run_scenario().await;
    let (mut want, mut got) = (Vec::new(), Vec::new());
    for (name, case) in &expected {
        engine_fields(name, case, &mut want);
        engine_fields(name, &actual[name], &mut got);
    }
    assert!(!want.is_empty(), "the golden has no engine fields");
    assert_eq!(got, want);
    assert_eq!(actual[ENGINE_CASE], expected[ENGINE_CASE], "JSON.parse messages of the engine itself");
}

/// The child's exit code, or `Err` after `limit`. The child leads its own process group, so on a timeout that
/// group is killed while the child is not yet reaped and its id cannot name another group.
fn wait(child: &mut Child, limit: Duration) -> Result<Option<i32>, Duration> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Ok(status.code());
        }
        if started.elapsed() > limit {
            let group = rustix::process::Pid::from_child(child);
            let killed = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            let _ = child.wait();
            println!("killed process group {} after {limit:?}: {killed:?}", group.as_raw_nonzero());
            return Err(limit);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The last `limit` bytes of `path`, for failure output.
fn tail(path: &Path, limit: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(limit)..]).into_owned()
}

/// One child case in a `pi -p` child of its own, compared with upstream's Node child of the same mode.
fn child_case(mode: &str) {
    let mut expected = golden(EXIT, "project_trust_store_exit.json");
    let expected = expected.remove(mode).unwrap_or_else(|| panic!("no {mode} case in project_trust_store_exit.json"));
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    cli_support::setup(&root, "http://127.0.0.1:9/v1");
    let work = root.join("exit-work");
    std::fs::create_dir_all(&work).unwrap();
    let (agent, report) = (work.join("agent"), work.join("report.json"));
    let options =
        json!({"agentDir": agent.display().to_string(), "reportPath": report.display().to_string(), "mode": mode});
    std::fs::write(root.join("exit.ts"), EXIT_EXTENSION.replace("__OPTIONS__", &options.to_string())).unwrap();
    std::fs::write(root.join("project_trust_store_exit.mjs"), EXIT_MODULE).unwrap();
    let extension = root.join("exit.ts").display().to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    cli_support::isolate(&mut command, &root)
        .args(["-e", &extension, "--model", "mock/mock-model", "--no-session", "-p", "/trustexit"])
        .current_dir(root.join("work"))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(root.join("pi.stdout")).unwrap())
        .stderr(std::fs::File::create(root.join("pi.stderr")).unwrap());
    let mut child = command.spawn().unwrap();
    let waited = wait(&mut child, Duration::from_secs(60));
    let clean = |text: String| text.replace(&work.display().to_string(), "<work>");
    let exists = |path: &Path| path.exists();
    let parsed = std::fs::read_to_string(&report).ok().map(clean);
    let parsed = parsed.map(|text| serde_json::from_str::<Value>(&text).unwrap());
    let code = waited.unwrap_or(None);
    let actual = json!({
        "exitCode": code,
        "report": parsed,
        "returned": exists(&work.join("report.json.returned")),
        "finally": exists(&work.join("report.json.finally")),
        "lockAfterExit": exists(&agent.join("trust.json.lock")),
        "trustFileAfterExit": exists(&agent.join("trust.json")),
    });
    if waited.is_ok() && actual == expected {
        return;
    }
    let kept = dir.keep();
    let (stdout, stderr) = (kept.join("pi.stdout"), kept.join("pi.stderr"));
    let evidence = format!(
        "evidence kept in {}\n{} tail:\n{}\n{} tail:\n{}",
        kept.display(),
        stdout.display(),
        tail(&stdout, 4096),
        stderr.display(),
        tail(&stderr, 4096)
    );
    assert!(waited.is_ok(), "{mode}: pi did not exit within {:?}\n{evidence}", waited.unwrap_err());
    assert_eq!(actual, expected, "{mode}\n{evidence}");
}

/// A getter calls process.exit inside the lock: upstream runs the user `exit` listener with the lock held, runs no
/// `finally`, and removes the lock afterwards.
#[test]
fn process_exit_inside_the_lock_matches_upstream() {
    child_case("exit");
}

/// A getter calls process.reallyExit inside the lock: upstream skips the `exit` listeners and still removes the lock.
#[test]
fn process_really_exit_inside_the_lock_matches_upstream() {
    child_case("reallyExit");
}

/// A getter removes the lock dir and re-enters the store: the outer release gives ENOTACQUIRED, and the outer update
/// timer's ECOMPROMISED later arrives as an uncaught exception, which the child waits for with a bound.
#[test]
fn a_compromised_outer_lock_matches_upstream() {
    child_case("compromised");
}
