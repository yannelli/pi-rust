//! node:worker_threads: workers on their own runtime threads, message ports, and broadcast
//! channels.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn worker_round_trip_error_and_terminate() {
    let (_dir, root) = tempdir();
    write(
        &root,
        "echo.js",
        r#"
const { parentPort, workerData, isMainThread, threadId, getEnvironmentData } = require("worker_threads");
parentPort.postMessage({ workerData, isMainThread, threadId: typeof threadId, env: getEnvironmentData("k"), argv: process.argv.slice(2), shared: process.env.PI_SHARED });
process.env.PI_SHARED = "from-worker";
parentPort.on("message", (message) => {
  if (message === "bye") return parentPort.close();
  parentPort.postMessage({ doubled: message.n * 2, bytes: message.bytes.length, date: message.date instanceof Date });
});
"#,
    );
    write(&root, "throws.js", "setTimeout(() => { throw new RangeError('boom'); }, 1);");
    write(&root, "spin.js", "require('worker_threads').parentPort.postMessage('spinning'); for (;;) {}");
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const { Worker, setEnvironmentData, SHARE_ENV, isMainThread, threadId } = require("worker_threads");
const path = require("path");
const out = { main: [isMainThread, threadId] };
setEnvironmentData("k", "env-data");
process.env.PI_SHARED = "from-main";
const worker = new Worker(path.join(__dirname, "echo.js"), { workerData: { a: [1, 2], m: new Map([[1, 2]]) }, argv: ["x"], env: SHARE_ENV });
const events = [];
worker.on("online", () => events.push("online"));
const first = await new Promise((resolve) => worker.once("message", resolve));
out.first = { ...first, workerData: { a: first.workerData.a, m: [...first.workerData.m] } };
worker.postMessage({ n: 21, bytes: new Uint8Array(5), date: new Date() });
out.second = await new Promise((resolve) => worker.once("message", resolve));
out.sharedBack = process.env.PI_SHARED;
worker.postMessage("bye");
out.exit = await new Promise((resolve) => worker.once("exit", resolve));
out.events = events;

const failing = new Worker("./throws.js".replace(".", __dirname));
const error = await new Promise((resolve) => failing.once("error", resolve));
out.error = [error.name, error.message, error instanceof RangeError];
out.errorExit = await new Promise((resolve) => failing.once("exit", resolve));

const spin = new Worker(path.join(__dirname, "spin.js"));
await new Promise((resolve) => spin.once("message", resolve));
out.terminated = await spin.terminate();

const evaled = new Worker("require('worker_threads').parentPort.postMessage(require('path').basename(__filename)); process.exit(7)", { eval: true });
out.eval = await new Promise((resolve) => evaled.once("message", resolve));
evaled.on("error", (e) => (out.evalError = e.message));
out.evalExit = await new Promise((resolve) => evaled.once("exit", resolve));
try { new Worker("relative.js"); } catch (e) { out.badPath = e.code; }
const piped = new Worker("console.log('to-stdout')", { eval: true, stdout: true });
let text = "";
piped.stdout.on("data", (chunk) => (text += chunk));
await new Promise((resolve) => piped.once("exit", resolve));
out.stdout = text;
return out;
"#,
    )
    .await;
    assert_eq!(
        actual,
        json!({
            "main": [true, 0],
            "first": { "workerData": { "a": [1, 2], "m": [[1, 2]] }, "isMainThread": false, "threadId": "number", "env": "env-data", "argv": ["x"], "shared": "from-main" },
            "second": { "doubled": 42, "bytes": 5, "date": true },
            "sharedBack": "from-worker",
            "exit": 0,
            "events": ["online"],
            "error": ["RangeError", "boom", true],
            "errorExit": 1,
            "terminated": 1,
            "eval": "[worker eval]",
            "evalExit": 7,
            "badPath": "ERR_WORKER_PATH",
            "stdout": "to-stdout\n"
        })
    );
}

#[tokio::test]
async fn message_channels_and_broadcast() {
    let (_dir, root) = tempdir();
    write(
        &root,
        "port.js",
        r#"
const { parentPort, workerData } = require("worker_threads");
const { port } = workerData;
port.on("message", (message) => {
  port.postMessage({ length: message.byteLength, view: message.view[0] });
  port.close();
});
const bc = new BroadcastChannel("news");
bc.onmessage = (event) => { parentPort.postMessage("worker heard " + event.data); bc.close(); };
parentPort.postMessage("ready");
"#,
    );
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const { Worker, MessageChannel, receiveMessageOnPort, markAsUntransferable } = require("worker_threads");
const out = {};
const { port1, port2 } = new MessageChannel();
const worker = new Worker(require("path").join(__dirname, "port.js"), { workerData: { port: port2 }, transferList: [port2] });
const heard = [];
const ready = new Promise((resolve) => worker.on("message", (message) => message === "ready" ? resolve() : heard.push(message)));
await ready;
const buffer = new ArrayBuffer(8);
const view = new Uint8Array(buffer);
view[0] = 9;
port1.postMessage({ byteLength: buffer.byteLength, view }, [buffer]);
out.detached = buffer.byteLength;
out.reply = await new Promise((resolve) => port1.once("message", resolve));
await new Promise((resolve) => port1.once("close", resolve));
const bc = new BroadcastChannel("news");
bc.postMessage("hello");
await new Promise((resolve) => worker.once("exit", resolve));
bc.close();
out.heard = heard;

const local = new MessageChannel();
local.port1.postMessage({ x: 1 });
local.port1.postMessage(new Set([2]));
out.received = [receiveMessageOnPort(local.port2).message, [...receiveMessageOnPort(local.port2).message], receiveMessageOnPort(local.port2)];
const marked = new ArrayBuffer(1);
markAsUntransferable(marked);
try { local.port1.postMessage(marked, [marked]); } catch (e) { out.untransferable = e.name; }
try { local.port1.postMessage(() => 1); } catch (e) { out.cloneError = e.name; }
try { local.port1.postMessage(1, [local.port1]); } catch (e) { out.sourcePort = e.message; }
const viaEvent = new Promise((resolve) => local.port2.addEventListener("message", (event) => resolve(event.data)));
local.port2.start();
local.port1.postMessage("evt");
out.event = await viaEvent;
local.port1.close();
const clone = structuredClone({ d: new Date(0), e: new TypeError("t"), big: 5n });
out.clone = [clone.d instanceof Date, clone.e instanceof TypeError, clone.big === 5n];
return out;
"#,
    )
    .await;
    assert_eq!(
        actual,
        json!({
            "detached": 0,
            "reply": { "length": 8, "view": 9 },
            "heard": ["worker heard hello"],
            "received": [{ "x": 1 }, [2], null],
            "untransferable": "DataCloneError",
            "cloneError": "DataCloneError",
            "sourcePort": "Transfer list contains source port",
            "event": "evt",
            "clone": [true, true, true]
        })
    );
}
