"use strict";
// Main thread of the pi Node backend. It owns the loopback connection to Rust (one JSON message
// per line) and relays messages to the Worker that runs extension code. It does not block.
const net = require("node:net");
const path = require("node:path");
const { Worker, MessageChannel } = require("node:worker_threads");

const port = Number(process.env.PI_NODE_BACKEND_PORT);
const token = process.env.PI_NODE_BACKEND_TOKEN;
delete process.env.PI_NODE_BACKEND_PORT;
delete process.env.PI_NODE_BACKEND_TOKEN;
const socket = net.connect({ host: "127.0.0.1", port });
socket.setNoDelay(true);
socket.write(token + "\n");

const MIN_MAJOR = 22;
let worker;
let ctl;
let replyPort;
let hooksPort;
let exitAnnounced = false;

function send(text) {
  socket.write(text + "\n");
}

function sendJson(frame) {
  send(JSON.stringify(frame));
}

function fail(message) {
  sendJson({ t: "fatal", error: { name: "Error", message, code: "ERR_PI_NODE_START" } });
}

function wakeWorker() {
  Atomics.store(ctl, 0, 1);
  Atomics.notify(ctl, 0);
}

function onRust(frame) {
  switch (frame.t) {
    case "init":
      start(frame.config);
      break;
    case "reply":
      if (typeof frame.id === "string" && frame.id.startsWith("h")) {
        hooksPort.postMessage(frame);
      } else {
        replyPort.postMessage(frame);
        wakeWorker();
      }
      break;
    case "interrupt":
      exitAnnounced = true;
      worker?.terminate();
      break;
    case "shutdown":
      process.exit(0);
      break;
    default:
      worker?.postMessage(frame);
  }
}

function start(config) {
  const major = Number(process.versions.node.split(".")[0]);
  if (major < MIN_MAJOR) return fail(`Node ${process.version} is older than Node ${MIN_MAJOR}`);
  ctl = new Int32Array(new SharedArrayBuffer(4));
  const replies = new MessageChannel();
  const hooks = new MessageChannel();
  replyPort = replies.port1;
  hooksPort = hooks.port1;
  hooksPort.on("message", (frame) => sendJson(frame));
  const resourceLimits = {};
  if (config.memoryMb) resourceLimits.maxOldGenerationSizeMb = config.memoryMb;
  if (config.stackMb) resourceLimits.stackSizeMb = config.stackMb;
  worker = new Worker(path.join(__dirname, "worker.cjs"), {
    env: config.env ?? process.env,
    stdout: true,
    stderr: true,
    resourceLimits,
    workerData: { ctl, replyPort: replies.port2, hooksPort: hooks.port2, config },
    transferList: [replies.port2, hooks.port2],
  });
  worker.stdout.on("data", (chunk) => sendJson({ t: "out", s: 1, b: chunk.toString("base64") }));
  worker.stderr.on("data", (chunk) => sendJson({ t: "out", s: 2, b: chunk.toString("base64") }));
  worker.on("message", (text) => send(text));
  worker.on("error", (error) => {
    sendJson({ t: "uncaught", kind: "exception", error: { name: error?.name ?? "Error", message: String(error?.message ?? error), stack: error?.stack } });
  });
  worker.on("exit", (code) => {
    // Output the worker wrote just before it ended arrives after the exit event.
    setImmediate(() => sendJson({ t: "ended", code, announced: exitAnnounced }));
  });
}

let buffered = "";
socket.setEncoding("utf8");
socket.on("data", (chunk) => {
  buffered += chunk;
  let newline;
  while ((newline = buffered.indexOf("\n")) >= 0) {
    const line = buffered.slice(0, newline);
    buffered = buffered.slice(newline + 1);
    if (line.trim()) onRust(JSON.parse(line));
  }
});
socket.on("end", () => process.exit(0));
socket.on("close", () => process.exit(0));
socket.on("error", () => process.exit(1));
