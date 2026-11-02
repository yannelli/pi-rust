//! Sender recovery for handles sent over IPC, against a child that answers the handle protocol itself: a NACKed
//! socket is sent again up to 3 times, later messages wait behind it, and a disconnect waits for its ACK.

mod common;
mod node_common;

use common::*;
use serde_json::{Value, json};

/// Replaces the built-in protocol listener: records what arrives, closes each descriptor, and NACKs the first
/// `argv[3]` deliveries of "A". In `preack` mode it sends "pre-ack" before each ACK. "ready" says it is in place.
const CHILD: &str = r#"
const mode = process.argv[2];
const nacks = { A: Number(process.argv[3] || 0) };
const arrivals = [];
const unwrap = (message) => {
  while (message && message.cmd === "NODE_HANDLE") message = message.msg;
  return message;
};
process.removeAllListeners("internalMessage");
process.on("internalMessage", (message, handle) => {
  if (!message || message.cmd !== "NODE_HANDLE") return;
  const value = unwrap(message);
  arrivals.push(`${value}${handle ? "+handle" : ""}`);
  if (handle) handle.close();
  if (mode === "preack") process.send("pre-ack");
  const nack = (nacks[value] || 0) > 0;
  if (nack) nacks[value]--;
  process.send({ cmd: nack ? "NODE_HANDLE_NACK" : "NODE_HANDLE_ACK" });
});
process.on("message", (message, handle) => {
  // A runtime that adopts handles before user listeners delivers a socket here; close it so its client ends.
  if (handle) handle.destroy();
  arrivals.push(String(message));
  if (message === "report") process.send({ report: arrivals });
});
process.send("ready");
"#;

/// Accepts two loopback connections and returns the client and the accepted socket of each, in order.
const CONNECT: &str = r#"
const { fork } = require("child_process");
const net = require("net");
const fs = require("fs");
const path = require("path");
const mark = (text) => fs.appendFileSync(path.join(__dirname, "stage"), ` | ${text}`);
fs.writeFileSync(path.join(__dirname, "stage"), "CASE");
const server = net.createServer();
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();
const open = async () => {
  const accepted = new Promise((resolve) => server.once("connection", resolve));
  const client = net.connect(port, "127.0.0.1");
  const ended = new Promise((resolve) => {
    client.once("end", () => resolve(true));
    client.once("error", (error) => resolve(error.code));
  });
  return { client, socket: await accepted, ended };
};
// The parent sends only after "ready", so a child's built-in protocol listener is already replaced.
const ready = (child) => new Promise((resolve) => child.once("message", resolve));
const reaped = (child) => new Promise((resolve, reject) => {
  child.once("error", reject);
  // Fork stdio is inherited; Node may emit only exit after a parent-initiated IPC disconnect.
  child.once("exit", (code, signal) => resolve({ code, signal }));
});
const finish = (connections) => {
  for (const { client } of connections) client.destroy();
  server.close();
};
"#;

/// Sends A (NACKed `NACKS` times), "mid" and B with callbacks, then "report"; `child.send` holds the later
/// messages back until A is settled.
const RETRY: &str = r#"
const warnings = [];
const onWarning = (warning) => warnings.push(warning.name);
process.on("warning", onWarning);
try {
  const a = await open();
  const b = await open();
  const child = fork(path.join(__dirname, "child.js"), ["nack", "NACKS"]);
  const exit = reaped(child);
  mark(`child ${await ready(child)}`);
  const calls = { A: 0, mid: 0, B: 0 };
  const counted = (name) => (error) => {
    if (!error) calls[name]++;
  };
  const report = new Promise((resolve) => child.on("message", (message) => message.report && resolve(message.report)));
  child.send("A", a.socket, counted("A"));
  child.send("mid", counted("mid"));
  child.send("B", b.socket, counted("B"));
  child.send("report");
  mark("sent");
  const arrivals = await report;
  mark("report");
  const ended = [await a.ended, await b.ended];
  mark("ended");
  child.disconnect();
  const { code, signal } = await exit;
  mark("reaped");
  finish([a, b]);
  return { arrivals, calls, warnings: [...warnings], ended, code, signal };
} finally {
  process.removeListener("warning", onWarning);
}
"#;

/// Sends A and disconnects at once: the channel stays open for the child's "pre-ack" until A's ACK.
const DISCONNECT: &str = r#"
const a = await open();
const child = fork(path.join(__dirname, "child.js"), ["preack"]);
const exit = reaped(child);
mark(`child ${await ready(child)}`);
const messages = [];
child.on("message", (message) => messages.push(message));
const disconnected = new Promise((resolve) => child.once("disconnect", () => resolve([...messages])));
child.send("A", a.socket);
child.disconnect();
const connected = child.connected;
mark("disconnect requested");
const beforeDisconnect = await disconnected;
mark("disconnected");
const ended = await a.ended;
const { code, signal } = await exit;
mark("reaped");
finish([a]);
return { connected, beforeDisconnect, ended, code, signal };
"#;

/// QuickJS only: closing the channel right after `send` makes the deferred handle write fail. The client's `end`
/// shows that no sender copy (the sent one or the kept one) is left open.
const FAILED_WRITE: &str = r#"
const a = await open();
const child = fork(path.join(__dirname, "child.js"), ["nack", "0"]);
const exit = reaped(child);
mark(`child ${await ready(child)}`);
const failed = new Promise((resolve) => child.send("A", a.socket, (error) => resolve(error ? error.code : null)));
child._closeChannel();
const error = await failed;
mark(`failed ${error}`);
const ended = await a.ended;
mark("ended");
const { code, signal } = await exit;
mark("reaped");
finish([a]);
return { error, pending: child._pendingMessage, queue: child._handleQueue, ended, code, signal };
"#;

fn retry_case(nacks: usize) -> (String, Value) {
    let deliveries = nacks.min(3) + 1;
    let arrivals: Vec<&str> = vec!["A+handle"; deliveries].into_iter().chain(["mid", "B+handle", "report"]).collect();
    let warnings: Vec<&str> = if nacks > 3 { vec!["SentHandleNotReceivedWarning"] } else { vec![] };
    let body =
        format!("{CONNECT}{RETRY}").replace("CASE", &format!("retry {nacks}")).replace("NACKS", &nacks.to_string());
    let expected = json!({
        "arrivals": arrivals,
        "calls": { "A": deliveries, "mid": 1, "B": 1 },
        "warnings": warnings,
        "ended": [true, true],
        "code": 0,
        "signal": null
    });
    (body, expected)
}

async fn run(runtime: &pi_js::JsRuntime, root: &std::path::Path, backend: &str) -> usize {
    write(root, "child.js", CHILD);
    let mut passed = 0;
    // With 4 NACKs the 4th reaches the limit: A's copy is closed with a warning and the queue goes on.
    for nacks in [0, 1, 4] {
        let (body, expected) = retry_case(nacks);
        let actual = run_cjs_within(runtime, root, &body, 60).await;
        assert_same_keys(&actual, &expected);
        println!("test handle_retry_{nacks}_nacks ({backend}) ... ok");
        passed += 1;
    }
    let body = format!("{CONNECT}{DISCONNECT}").replace("CASE", "disconnect");
    let actual = run_cjs_within(runtime, root, &body, 60).await;
    let expected =
        json!({ "connected": false, "beforeDisconnect": ["pre-ack"], "ended": true, "code": 0, "signal": null });
    assert_same_keys(&actual, &expected);
    println!("test disconnect_waits_for_the_ack ({backend}) ... ok");
    // Unix only: a Windows duplicate the peer never imported is not shown to release the socket.
    if backend == "quickjs" && cfg!(unix) {
        let body = format!("{CONNECT}{FAILED_WRITE}").replace("CASE", "failed write");
        let actual = run_cjs_within(runtime, root, &body, 60).await;
        let expected = json!({
            "error": "ERR_INVALID_STATE",
            "pending": null,
            "queue": null,
            "ended": true,
            "code": 0,
            "signal": null
        });
        assert_same_keys(&actual, &expected);
        println!("test failed_handle_write_closes_every_copy ({backend}) ... ok");
        return passed + 2;
    }
    passed + 1
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
    let mut passed = run(&quickjs, &root, "quickjs").await;
    if let Some(node) = node_common::node_runtime_with(options) {
        passed += run(&node, &root, "node").await;
    }
    passed
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
    // A hung child would block CI until its job times out; fail with the last step reached instead.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(240));
        let stage = STAGE.get().and_then(|path| std::fs::read_to_string(path).ok()).unwrap_or_default();
        let (out, err) = CAPTURE.get().map(|capture| (capture.stdout(), capture.stderr())).unwrap_or_default();
        eprintln!("test ipc_recovery ... FAILED: timed out at: {stage}\nstdout: {out}\nstderr: {err}");
        std::process::exit(101);
    });
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let passed = runtime.block_on(cases());
    println!("\ntest result: ok. {passed} passed; 0 failed; 0 ignored");
}
