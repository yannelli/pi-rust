//! Windows: a plain child started while a TCP listener is open must not keep the port listening after the server
//! closes. QuickJS and the Node backend run the same steps; CI installs Node 22, so a missing Node backend fails.

mod common;
mod node_common;

use std::sync::Arc;

use common::*;
use serde_json::{Value, json};

/// Bounded snapshots record each event with a timestamp. A "pending" connect is a failure, never a success.
const PARENT: &str = r#"
const net = require("net");
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");
const t0 = Date.now();
const events = [];
const mark = (text) => {
  events.push(`${Date.now() - t0}ms ${text}`);
  fs.appendFileSync(path.join(__dirname, "stage"), ` | ${text}`);
};
fs.writeFileSync(path.join(__dirname, "stage"), "listener");
const within = (promise, ms) => {
  let timer;
  const bound = new Promise((resolve) => (timer = setTimeout(() => resolve("pending"), ms)));
  return Promise.race([promise, bound]).finally(() => clearTimeout(timer));
};
const meta = {
  platform: process.platform, node: process.versions.node, startedAt: new Date(t0).toISOString(),
  bounds: { listenMs: 10000, readyMs: 15000, closeMs: 10000, connectMs: 15000, exitMs: 15000, cleanupMs: 5000 },
};
const plainScript = 'process.stdin.on("end", () => process.exit(0)); process.stdin.resume(); process.stdout.write("ready");';
const servers = [];
const sockets = [];
const result = {};
let plain, plainExit, plainEnded = false, plainExitSeen = false;
const endPlain = () => plainEnded || ((plainEnded = true), plain.stdin.end());
const listen = async () => {
  const server = net.createServer();
  servers.push(server);
  const listening = new Promise((resolve) => {
    server.once("error", (error) => resolve(`error ${error.code}`));
    server.listen(0, "127.0.0.1", () => resolve("listening"));
  });
  const state = await within(listening, 10000);
  if (state !== "listening") throw new Error(`listen gave ${state}`);
  return server;
};
const close = async (server) => {
  const closed = new Promise((resolve) => server.close((error) => resolve(error ? `error ${error.code}` : "closed")));
  const state = await within(closed, 10000);
  if (state !== "closed") throw new Error(`server.close gave ${state}`);
};
const attempt = (port) => {
  const socket = net.connect(port, "127.0.0.1");
  sockets.push(socket);
  const first = new Promise((resolve) => {
    socket.once("connect", () => resolve("connect"));
    socket.once("error", (error) => resolve(`error ${error.code}`));
  });
  return within(first, 15000);
};
try {
  const control = await listen();
  const controlPort = control.address().port;
  await close(control);
  result.controlNoSpawn = await attempt(controlPort);
  mark(`control connect with no child: ${result.controlNoSpawn}`);

  const server = await listen();
  const { port } = server.address();
  mark(`listening on ${port}`);
  plain = spawn("node", ["-e", plainScript], { stdio: ["pipe", "pipe", "pipe"] });
  plainExit = new Promise((resolve) => plain.once("exit", (code, signal) => {
    plainExitSeen = true;
    mark(`plain child exit ${code} ${signal}`);
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
  await close(server);
  mark("server closed");
  result.whilePlainAlive = await attempt(port);
  mark(`connect while the plain child runs: ${result.whilePlainAlive}`);
  endPlain();
  result.plainExited = await within(plainExit, 15000);
  if (result.plainExited === "pending") throw new Error("the plain child did not exit within 15 s of stdin EOF");
  result.afterPlainExit = await attempt(port);
  mark(`connect after the plain exit: ${result.afterPlainExit}`);
  return { ...result, meta, events };
} catch (error) {
  return { error: String((error && error.stack) || error), ...result, meta, events };
} finally {
  for (const socket of sockets) socket.destroy();
  for (const server of servers) if (server.listening) server.close();
  if (plain && !plainExitSeen) {
    endPlain();
    if ((await within(plainExit, 5000)) === "pending") {
      plain.kill();
      mark("cleanup killed the plain child");
      if ((await within(plainExit, 5000)) === "pending") mark("cleanup: no plain exit within 5 s of the kill");
    }
  }
}
"#;

/// The script returns its failures as JSON; `run_cjs_within` panics only on a failed import or call, or a timeout.
async fn run(runtime: &pi_js::JsRuntime, root: &std::path::Path, backend: &str) -> Value {
    let result = run_cjs_within(runtime, root, PARENT, 120).await;
    println!("listener-inheritance {backend} | {result}");
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_listener_is_not_kept_open_by_a_later_child() {
    if !cfg!(windows) {
        println!("skipping: a Windows-only check of listener inheritance");
        return;
    }
    let (_dir, root) = tempdir();
    let capture = Arc::new(Capture::default());
    let mut options = options(&root, &capture);
    options.env = Some(std::env::vars().collect());
    let quickjs = pi_js::JsRuntime::new(options.clone()).unwrap();
    let mut results = vec![("quickjs", run(&quickjs, &root, "quickjs").await)];
    let node = match node_common::node_runtime_with(options) {
        Some(node) => run(&node, &root, "node").await,
        None => json!({ "error": "no Node 22 on PATH or PI_NODE_PATH" }),
    };
    results.push(("node", node));
    let refused = json!("error ECONNREFUSED");
    let failed: Vec<&(&str, Value)> = results
        .iter()
        .filter(|(_, value)| {
            value.get("error").is_some()
                || value["controlNoSpawn"] != refused
                || value["whilePlainAlive"] != refused
                || value["plainExited"] != json!({ "code": 0, "signal": null })
                || value["afterPlainExit"] != refused
        })
        .collect();
    assert!(failed.is_empty(), "a closed listener accepted a connection or a step failed: {failed:#?}");
}
