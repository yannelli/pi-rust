//! Windows diagnostic: which process keeps a TCP connection open after its sender lets go. It separates a child that
//! inherited the accepted socket from one that holds a transferred copy, on QuickJS and on real Node. Evidence only.

mod common;
mod node_common;

use common::*;
use serde_json::{Value, json};

/// "idle" only answers "ready"; "transfer" replaces the protocol listener, closes the received socket, then ACKs.
const CHILD: &str = r#"
process.on("message", () => {});
if (process.argv[2] === "transfer") {
  process.removeAllListeners("internalMessage");
  process.on("internalMessage", (message, handle) => {
    if (!message || message.cmd !== "NODE_HANDLE") return;
    if (handle) handle.close();
    process.send({ cmd: "NODE_HANDLE_ACK" });
    process.send("acked");
  });
}
process.send("ready");
"#;

/// Bounded snapshots mark each end as measure or cleanup. Pending is not proof of absence.
/// The plain child exits on stdin EOF while the IPC child stays connected.
const PARENT: &str = r#"
const { fork, spawn } = require("child_process");
const net = require("net");
const fs = require("fs");
const path = require("path");
const t0 = Date.now();
const events = [];
const mark = (text) => {
  events.push(`${Date.now() - t0}ms ${text}`);
  fs.appendFileSync(path.join(__dirname, "stage"), ` | ${text}`);
};
fs.writeFileSync(path.join(__dirname, "stage"), "CASE");
const within = (promise, ms) => {
  let timer;
  const bound = new Promise((resolve) => (timer = setTimeout(() => resolve("pending"), ms)));
  return Promise.race([promise, bound]).finally(() => clearTimeout(timer));
};
const meta = {
  platform: process.platform, node: process.versions.node, startedAt: new Date(t0).toISOString(),
  bounds: { readyMs: 10000, ackedMs: 10000, snapshotMs: 2000, exitMs: 20000, endAfterExitMs: 10000, cleanupMs: 5000 },
  plainBounds: { readyMs: 15000, exitMs: 15000, endAfterPlainExitMs: 10000 },
  note: "beforeDisconnect is a bounded snapshot, not proof of absence",
};
let phase = "measure";
let server, client, child, exit;
let exitSeen = false;
let plain, plainExit, plainEnded = false, plainExitSeen = false;
const plainResult = {};
const plainScript = 'process.stdin.on("end", () => process.exit(0)); process.stdin.resume(); process.stdout.write("ready");';
const endPlain = () => plainEnded || ((plainEnded = true), plain.stdin.end());
try {
  server = net.createServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  const start = async () => {
    child = fork(path.join(__dirname, "diag_child.js"), [TRANSFER ? "transfer" : "idle"]);
    exit = new Promise((resolve) => child.once("exit", (code, signal) => {
      exitSeen = true;
      mark(`child exit ${code} ${signal} (${phase})`);
      resolve({ code, signal });
    }));
    child.once("disconnect", () => mark(`parent saw disconnect (${phase})`));
    const ready = await within(new Promise((resolve) => child.once("message", resolve)), 10000);
    if (ready === "pending") throw new Error("the child sent nothing within 10 s");
    mark(`child ${ready}`);
  };
  if (!ACCEPT_FIRST) await start();
  const accepted = new Promise((resolve) => server.once("connection", resolve));
  client = net.connect(port, "127.0.0.1");
  const ended = new Promise((resolve) => {
    client.once("end", () => (mark(`client end (${phase})`), resolve("end")));
    client.once("error", (error) => (mark(`client error ${error.code} (${phase})`), resolve(error.code)));
  });
  const socket = await within(accepted, 10000);
  if (socket === "pending") throw new Error("no connection within 10 s");
  mark("accepted");
  if (ACCEPT_FIRST) await start();
  if (PLAIN) {
    plain = spawn("node", ["-e", plainScript], { stdio: ["pipe", "pipe", "pipe"] });
    plainExit = new Promise((resolve) => plain.once("exit", (code, signal) => {
      plainExitSeen = true;
      mark(`plain child exit ${code} ${signal} (${phase})`);
      resolve({ code, signal });
    }));
    const plainReady = new Promise((resolve) => {
      let text = "";
      plain.stdout.on("data", (chunk) => {
        text += String(chunk);
        if (text === "ready") resolve(text);
      });
      plain.stdout.once("end", () => resolve(`EOF ${text}`));
      plain.once("error", (error) => resolve(`error ${error.code}`));
    });
    const said = await within(plainReady, 15000);
    if (said !== "ready") throw new Error(`the plain child gave ${said} within 15 s`);
    mark("plain child ready");
  }
  if (TRANSFER) {
    const acked = new Promise((resolve) => child.on("message", (message) => message === "acked" && resolve("acked")));
    child.send("A", socket, (error) => mark(`send callback ${error ? error.code : "ok"}`));
    if ((await within(acked, 10000)) === "pending") throw new Error("the child did not ACK within 10 s");
    mark("child acked");
  } else {
    socket.destroy();
    mark("parent destroyed the accepted socket");
  }
  if (PLAIN) {
    plainResult.beforePlainExit = await within(ended, 2000);
    endPlain();
    plainResult.plainExited = await within(plainExit, 15000);
    if (plainResult.plainExited === "pending") throw new Error("the plain child did not exit within 15 s of stdin EOF");
    if (plainResult.plainExited.code !== 0 || plainResult.plainExited.signal !== null) {
      throw new Error(`the plain child exited ${JSON.stringify(plainResult.plainExited)}`);
    }
    const { beforePlainExit } = plainResult;
    plainResult.afterPlainExit = beforePlainExit === "pending" ? await within(ended, 10000) : beforePlainExit;
    mark(`after the plain exit: ${plainResult.afterPlainExit}, IPC child connected ${child.connected}`);
  }
  const beforeDisconnect = await within(ended, 2000);
  child.disconnect();
  const exited = await within(exit, 20000);
  const afterExit = beforeDisconnect === "pending" ? await within(ended, 10000) : beforeDisconnect;
  return { beforeDisconnect, afterExit, exited, ...plainResult, meta, events };
} catch (error) {
  return { error: String((error && error.stack) || error), ...plainResult, meta, events };
} finally {
  phase = "cleanup";
  if (plain && !plainExitSeen) {
    endPlain();
    if ((await within(plainExit, 5000)) === "pending") {
      plain.kill();
      mark("cleanup killed the plain child");
      if ((await within(plainExit, 5000)) === "pending") mark("cleanup: no plain exit within 5 s of the kill");
    }
  }
  if (child && child.connected) {
    child.disconnect();
    mark("cleanup disconnected the child");
  }
  if (child && !exitSeen && (await within(exit, 5000)) === "pending") {
    child.kill();
    mark("cleanup killed the child");
    if ((await within(exit, 5000)) === "pending") mark("cleanup: no exit within 5 s of the kill");
  }
  if (client) client.destroy();
  if (server) server.close();
}
"#;

/// The child forked after or before the accept, with or without a transfer of the socket, and with or without a plain
/// child started after the accept.
const CASES: [(&str, bool, bool, bool); 5] = [
    ("accept, fork, destroy", true, false, false),
    ("fork, accept, destroy", false, false, false),
    ("accept, fork, transfer", true, true, false),
    ("fork, accept, transfer", false, true, false),
    ("fork, accept, plain child, transfer", false, true, true),
];

/// Like `common::run_cjs`, but a failed import or call becomes an `error` result, so every case and backend runs.
async fn run_case(runtime: &pi_js::JsRuntime, root: &std::path::Path, file: &str, body: &str) -> Value {
    write(root, file, &format!("module.exports = async function () {{\n{body}\n}};\n"));
    let function = match runtime.import_default(root.join(file).display().to_string()).await {
        Ok(function) => function,
        Err(error) => return json!({ "error": format!("import: {}", error.display_with_stack()) }),
    };
    match function.call(vec![]).await {
        Ok(value) => value.into_json(),
        Err(error) => json!({ "error": format!("call: {}", error.display_with_stack()) }),
    }
}

async fn run(runtime: &pi_js::JsRuntime, root: &std::path::Path, backend: &str) -> Vec<(String, Value)> {
    write(root, "diag_child.js", CHILD);
    let mut results = Vec::new();
    for (index, (name, accept_first, transfer, plain)) in CASES.into_iter().enumerate() {
        let body = PARENT
            .replace("CASE", name)
            .replace("ACCEPT_FIRST", &accept_first.to_string())
            .replace("TRANSFER", &transfer.to_string())
            .replace("PLAIN", &plain.to_string());
        let file = format!("diag-{backend}-{index}.cjs");
        let limit = std::time::Duration::from_secs(if plain { 150 } else { 90 });
        let result = match tokio::time::timeout(limit, run_case(runtime, root, &file, &body)).await {
            Ok(value) => value,
            Err(_) => {
                json!({ "error": format!("case exceeded {} s", limit.as_secs()), "stage": std::fs::read_to_string(root.join("stage")).ok() })
            }
        };
        println!("ipc-windows-diag {backend} | {name} | {result}");
        results.push((format!("{backend}: {name}"), result));
    }
    results
}

async fn cases() -> Vec<(String, Value)> {
    let (_dir, root) = tempdir();
    let capture = std::sync::Arc::new(Capture::default());
    let mut options = options(&root, &capture);
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.insert(GUARD.into(), "1".into());
    options.env = Some(env);
    let quickjs = pi_js::JsRuntime::new(options.clone()).unwrap();
    let mut results = run(&quickjs, &root, "quickjs").await;
    // CI installs Node 22, so a missing Node backend is recorded as a failed result.
    match node_common::node_runtime_with(options) {
        Some(node) => results.extend(run(&node, &root, "node").await),
        None => results.push(("node".into(), json!({ "error": "no Node 22 on PATH or PI_NODE_PATH" }))),
    }
    results
}

/// Set for children, so one that is not asked to run JavaScript cannot rerun the cases.
const GUARD: &str = "PI_JS_FORK_TEST_CHILD";

fn main() {
    if let Some(code) = pi_js::runner::child_main() {
        std::process::exit(code);
    }
    if std::env::var_os(GUARD).is_some() {
        std::process::exit(97);
    }
    if !cfg!(windows) {
        println!("skipping: a Windows-only diagnostic of socket inheritance");
        return;
    }
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let results = runtime.block_on(cases());
    // Every case must finish without an error, with the child exiting 0 and the client's end arriving at some point.
    let failed: Vec<&(String, Value)> = results
        .iter()
        .filter(|(_, value)| {
            value.get("error").is_some()
                || value["exited"] != json!({ "code": 0, "signal": null })
                || value["afterExit"] != json!("end")
        })
        .collect();
    assert!(failed.is_empty(), "diagnostic cases ended unexpectedly: {failed:#?}");
    assert_eq!(results.len(), 2 * CASES.len(), "both backends must run every case");
    println!("\ntest result: ok. {} passed; 0 failed; 0 ignored", results.len());
}
