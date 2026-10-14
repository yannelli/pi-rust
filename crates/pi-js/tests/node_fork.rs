//! child_process.fork and IPC, process.stdin, and `node`-style runs of process.execPath. The
//! forked children run this test binary, which runs JavaScript like `node` when asked.

mod common;

use common::*;
use serde_json::json;

const CHILD: &str = r#"
const fs = require("fs");
const mode = process.argv[2];
if (mode === "echo") {
  process.on("message", (message) => {
    process.send({ echo: message, argv: process.argv.slice(3), connected: process.connected, hasSend: typeof process.send });
    if (message === "bye") process.disconnect();
  });
} else if (mode === "exit") {
  process.send("leaving", () => process.exit(3));
} else if (mode === "advanced") {
  process.once("message", (message) => process.send(message));
} else if (mode === "stdin") {
  let text = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (chunk) => (text += chunk));
  process.stdin.on("end", () => process.stdout.write(text.toUpperCase() + `|${process.stdin.isTTY}`));
} else if (mode === "stdin-sync") {
  process.stdout.write(fs.readFileSync(0, "utf8").split("").reverse().join(""));
} else if (mode === "silent") {
  console.log("from child", typeof process.send);
  process.exitCode = 4;
}
"#;

async fn fork_cases() {
    let (_dir, root) = tempdir();
    let capture = std::sync::Arc::new(Capture::default());
    let mut options = options(&root, &capture);
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.insert(GUARD.into(), "1".into());
    options.env = Some(env);
    let runtime = pi_js::JsRuntime::new(options).unwrap();
    write(&root, "child.js", CHILD);
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const { fork, spawn, execFileSync } = require("child_process");
const path = require("path");
const script = path.join(__dirname, "child.js");
const events = [];
const child = fork(script, ["echo", "x"]);
const replies = [];
child.on("message", (message) => {
  replies.push(message);
  if (replies.length === 1) child.send("bye");
});
child.on("disconnect", () => events.push("disconnect"));
const sent = child.send("hi");
const closed = await new Promise((resolve) => child.on("close", (code, signal) => resolve([code, signal])));
const afterSend = await new Promise((resolve) => child.send("late", (error) => resolve(error.code)));
const disconnectError = await new Promise((resolve) => { child.once("error", (error) => resolve(error.code)); child.disconnect(); });

const exiting = fork(script, ["exit"]);
const exitMessages = [];
exiting.on("message", (message) => exitMessages.push(message));
const exitOrder = [];
exiting.on("disconnect", () => exitOrder.push("disconnect"));
exiting.on("exit", (code) => exitOrder.push(["exit", code]));
await new Promise((resolve) => exiting.on("close", resolve));

const advanced = fork(script, ["advanced"], { serialization: "advanced" });
const payload = { map: new Map([[1, { deep: [1n, undefined] }]]), date: new Date(0), set: new Set(["a"]), nan: NaN, buf: Buffer.from("hi") };
payload.self = payload;
advanced.send(payload);
const back = await new Promise((resolve) => advanced.once("message", resolve));
advanced.disconnect();
await new Promise((resolve) => advanced.on("close", resolve));

const silent = fork(script, ["silent"], { silent: true });
let silentOut = "";
silent.stdout.on("data", (chunk) => (silentOut += chunk));
const silentCode = await new Promise((resolve) => silent.on("close", resolve));

const reader = spawn(process.execPath, [script, "stdin"]);
let readerOut = "";
reader.stdout.on("data", (chunk) => (readerOut += chunk));
reader.stdin.write("abc ");
reader.stdin.end("def");
await new Promise((resolve) => reader.on("close", resolve));
const reversed = execFileSync(process.execPath, [script, "stdin-sync"], { input: "pi-js" }).toString();
const printed = execFileSync(process.execPath, ["-p", "[1 + 2, process.argv.length, typeof require]", "extra"]).toString();

const err = (fn) => { try { fn(); return "no error"; } catch (e) { return e.code; } };
const probe = fork(script, ["advanced"]);
const errors = [err(() => probe.send()), err(() => probe.send(Symbol("s"))), err(() => probe.send("x", {})),
  err(() => fork(script, [], { stdio: ["pipe", "pipe", "pipe"] })), err(() => fork(script, [], { serialization: "v8" }))];
probe.kill();
await new Promise((resolve) => probe.on("close", resolve));
return {
  sent, replies, closed, events, afterSend, disconnectError, exitMessages, exitOrder,
  back: [back.map instanceof Map, back.map.get(1).deep[0] === 1n, back.map.get(1).deep.length, back.date instanceof Date && back.date.getTime(), [...back.set], Number.isNaN(back.nan), Buffer.isBuffer(back.buf) && back.buf.toString(), back.self === back],
  silentOut, silentCode, readerOut, reversed, printed, errors, connected: child.connected,
};
"#,
    )
    .await;
    assert_eq!(actual["sent"], true);
    assert_eq!(
        actual["replies"],
        json!([
            {"echo": "hi", "argv": ["x"], "connected": true, "hasSend": "function"},
            {"echo": "bye", "argv": ["x"], "connected": true, "hasSend": "function"}
        ])
    );
    assert_eq!(actual["closed"], json!([0, null]));
    assert_eq!(actual["events"], json!(["disconnect"]));
    assert_eq!(actual["afterSend"], "ERR_IPC_CHANNEL_CLOSED");
    assert_eq!(actual["disconnectError"], "ERR_IPC_DISCONNECTED");
    assert_eq!(actual["exitMessages"], json!(["leaving"]));
    assert_eq!(actual["exitOrder"].as_array().unwrap().len(), 2);
    assert!(actual["exitOrder"].as_array().unwrap().contains(&json!(["exit", 3])));
    assert_eq!(actual["back"], json!([true, true, 2, 0, ["a"], true, "hi", true]));
    assert_eq!(actual["silentOut"], "from child function\n");
    assert_eq!(actual["silentCode"], 4);
    assert_eq!(actual["readerOut"], "ABC DEF|undefined");
    assert_eq!(actual["reversed"], "sj-ip");
    assert_eq!(actual["printed"], "[ 3, 2, 'function' ]\n");
    assert_eq!(
        actual["errors"],
        json!([
            "ERR_MISSING_ARGS",
            "ERR_INVALID_ARG_TYPE",
            "ERR_INVALID_HANDLE_TYPE",
            "ERR_CHILD_PROCESS_IPC_REQUIRED",
            "ERR_INVALID_ARG_VALUE"
        ])
    );
    assert_eq!(actual["connected"], false);
    assert_eq!(capture.stderr(), "");
}

/// Set for children, so one that is not asked to run JavaScript cannot rerun the tests.
const GUARD: &str = "PI_JS_FORK_TEST_CHILD";

fn main() {
    if let Some(code) = pi_js::runner::child_main() {
        std::process::exit(code);
    }
    if std::env::var_os(GUARD).is_some() {
        std::process::exit(97);
    }
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(fork_cases());
    println!("test fork_cases ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
