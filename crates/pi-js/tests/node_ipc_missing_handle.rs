//! Handle messages without a descriptor are rejected with a NACK instead of reaching user message listeners.

mod common;
mod node_common;

use common::*;
use serde_json::json;

const CHILD: &str = r#"
const mode = process.argv[2];
process.on("disconnect", () => {});
process.removeAllListeners("internalMessage");
if (mode === "nack") {
  process.on("internalMessage", (message) => {
    if (message.cmd === "NODE_HANDLE_NACK") {
      process.send("nack");
    }
  });
}
process.send("before");
const missing = { cmd: "NODE_HANDLE", type: "net.Socket", msg: "undeliverable" };
process.send(missing);
if (mode === "drop") process.send("after");
"#;

const PARENT: &str = r#"
const { fork } = require("child_process");
const fs = require("fs");
const path = require("path");
const mark = (text) => fs.appendFileSync(path.join(__dirname, "stage"), ` | ${text}`);
fs.writeFileSync(path.join(__dirname, "stage"), "fork MODE");
const mode = "MODE";
const child = fork(path.join(__dirname, "child.js"), [mode]);
const messages = [];
let disconnected = false;
child.on("message", (message) => {
  messages.push(message);
  mark(`message ${message}`);
  if (message === (mode === "drop" ? "after" : "nack")) child.disconnect();
});
child.on("disconnect", () => { disconnected = true; mark("disconnect"); });
const { code, signal } = await new Promise((resolve, reject) => {
  child.once("error", reject);
  // Fork stdio is inherited; Node may emit only exit after a parent-initiated IPC disconnect.
  child.once("exit", (code, signal) => resolve({ code, signal }));
});
mark("reaped");
return { messages, disconnected, code, signal };
"#;

async fn run(runtime: &pi_js::JsRuntime, root: &std::path::Path, backend: &str) {
    write(root, "child.js", CHILD);
    for (mode, messages) in [("drop", json!(["before", "after"])), ("nack", json!(["before", "nack"]))] {
        let actual = run_cjs_within(runtime, root, &PARENT.replace("MODE", mode), 60).await;
        assert_same_keys(&actual, &json!({ "messages": messages, "disconnected": true, "code": 0, "signal": null }));
        println!("test missing_handle_{mode} ({backend}) ... ok");
    }
}

async fn cases() -> usize {
    let (_dir, root) = tempdir();
    let _ = STAGE.set(root.join("stage"));
    let capture = std::sync::Arc::new(Capture::default());
    let _ = CAPTURE.set(capture.clone());
    let mut options = options(&root, &capture);
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.insert(GUARD.into(), "1".into());
    options.env = Some(env);
    let quickjs = pi_js::JsRuntime::new(options.clone()).unwrap();
    run(&quickjs, &root, "quickjs").await;
    if let Some(node) = node_common::node_runtime_with(options) {
        run(&node, &root, "node").await;
        4
    } else {
        2
    }
}

const GUARD: &str = "PI_JS_FORK_TEST_CHILD";
static STAGE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static CAPTURE: std::sync::OnceLock<std::sync::Arc<Capture>> = std::sync::OnceLock::new();

fn main() {
    if let Some(code) = pi_js::runner::child_main() {
        std::process::exit(code);
    }
    if std::env::var_os(GUARD).is_some() {
        std::process::exit(97);
    }
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(180));
        let stage = STAGE.get().and_then(|path| std::fs::read_to_string(path).ok()).unwrap_or_default();
        let (out, err) = CAPTURE.get().map(|capture| (capture.stdout(), capture.stderr())).unwrap_or_default();
        eprintln!("test missing_handle ... FAILED: timed out at: {stage}\nstdout: {out}\nstderr: {err}");
        std::process::exit(101);
    });
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let passed = runtime.block_on(cases());
    println!("\ntest result: ok. {passed} passed; 0 failed; 0 ignored");
}
