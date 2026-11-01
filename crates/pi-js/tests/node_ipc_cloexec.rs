//! A listening socket received over IPC is close-on-exec: a grandchild spawned while the receiver holds it
//! does not inherit it, so once every copy is closed, connecting is refused. Runs on QuickJS and on Node 22.

mod common;
mod node_common;

use common::*;
use serde_json::json;

/// The receiver: on "server" it spawns a waiting grandchild, closes the received server once the grandchild
/// says READY, and answers "ready"; on "end" it closes the grandchild's stdin and reports how it closed.
const HOLDER: &str = r#"
const { spawn } = require("child_process");
let grandchild;
process.on("message", (message, server) => {
  if (message === "server") {
    grandchild = spawn("sh", ["-c", "echo READY; read line"], { stdio: ["pipe", "pipe", "ignore"] });
    let out = "";
    let told = false;
    grandchild.stdout.on("data", (chunk) => {
      out += chunk;
      if (told || !out.includes("READY")) return;
      told = true;
      server.close(() => process.send("ready"));
    });
  } else if (message === "end") {
    grandchild.once("close", (code, signal) => process.send({ code, signal }));
    grandchild.stdin.end("done\n");
  }
});
"#;

const PARENT: &str = r#"
const { fork } = require("child_process");
const net = require("net");
const fs = require("fs");
const path = require("path");
const mark = (text) => fs.appendFileSync(path.join(__dirname, "stage"), ` | ${text}`);
fs.writeFileSync(path.join(__dirname, "stage"), "fork");
const child = fork(path.join(__dirname, "holder.js"));
const reply = () => new Promise((resolve) => child.once("message", resolve));
const server = net.createServer();
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();
mark("send server");
const ready = reply();
child.send("server", server);
const first = await ready;
mark(`child ${first}`);
await new Promise((resolve) => server.close(resolve));
mark("parent closed");
const outcome = await new Promise((resolve) => {
  const client = net.connect(port, "127.0.0.1");
  client.once("connect", () => {
    client.destroy();
    resolve("connected");
  });
  client.once("error", (error) => resolve(error.code));
});
mark(`connect ${outcome}`);
const reaped = reply();
child.send("end");
const grandchild = await reaped;
mark("grandchild closed");
const exited = new Promise((resolve) => child.once("exit", resolve));
child.disconnect();
const childCode = await exited;
mark("child exited");
return { first, outcome, grandchild, childCode };
"#;

/// The grandchild is reaped before the result returns, so a failing assertion leaves no process behind.
fn expected() -> serde_json::Value {
    json!({ "first": "ready", "outcome": "ECONNREFUSED", "grandchild": { "code": 0, "signal": null }, "childCode": 0 })
}

async fn run(runtime: &pi_js::JsRuntime, root: &std::path::Path, backend: &str) {
    write(root, "holder.js", HOLDER);
    let actual = run_cjs_within(runtime, root, PARENT, 60).await;
    assert_same_keys(&actual, &expected());
    println!("test received_listener_is_close_on_exec ({backend}) ... ok");
}

async fn cases() {
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
    }
}

/// Set for children, so one that is not asked to run JavaScript cannot rerun the tests.
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
    if cfg!(windows) {
        println!("skipping: Windows passes sockets with WSADuplicateSocketW, not SCM_RIGHTS, and has no `sh`");
        return;
    }
    // A hung child would block CI until its job times out; fail with the last step reached instead.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(180));
        let stage = STAGE.get().and_then(|path| std::fs::read_to_string(path).ok()).unwrap_or_default();
        let (out, err) = CAPTURE.get().map(|capture| (capture.stdout(), capture.stderr())).unwrap_or_default();
        eprintln!(
            "test received_listener_is_close_on_exec ... FAILED: timed out at: {stage}\nstdout: {out}\nstderr: {err}"
        );
        std::process::exit(101);
    });
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(cases());
    println!("\ntest result: ok");
}
