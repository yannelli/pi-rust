"use strict";
// The Worker that runs extension code. Requests from Rust arrive as messages; calls into Rust
// that must return a value block on Atomics.wait until the main thread posts the reply.
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const { parentPort, workerData, receiveMessageOnPort } = require("node:worker_threads");
const wire = require("./wire.cjs");
require("./worker_globals.cjs");

const { ctl, replyPort, hooksPort, config } = workerData;
let sequence = 0;
const pendingHost = new Map();

function post(text) {
  parentPort.postMessage(text);
}

/** Send `{t, ...fields}` plus raw JSON members, and wait for Rust's reply. */
function ask(fields, raw = "") {
  const id = `w${++sequence}`;
  Atomics.store(ctl, 0, 0);
  post(JSON.stringify({ ...fields, id }).slice(0, -1) + raw + "}");
  for (;;) {
    Atomics.wait(ctl, 0, 0, 50);
    Atomics.store(ctl, 0, 0);
    let message;
    while ((message = receiveMessageOnPort(replyPort))) {
      if (message.message.id === id) return message.message;
    }
  }
}

function answer(reply) {
  if (reply.err) throw wire.toError(reply.err);
  return reply.ok;
}

const host = {
  call(name, ...args) {
    const reply = ask({ t: "host", name }, `,"args":[${args.map(wire.encode).join(",")}]`);
    if (reply.err) throw wire.toError(reply.err);
    if (reply.async !== undefined) {
      return new Promise((resolve, reject) => pendingHost.set(reply.async, { resolve, reject }));
    }
    return wire.decode(reply.ok);
  },
  callAsync(name, ...args) {
    try {
      return Promise.resolve(host.call(name, ...args));
    } catch (error) {
      return Promise.reject(error);
    }
  },
  has: (name) => answer(ask({ t: "meta", op: "has", name })),
  names: () => answer(ask({ t: "meta", op: "names" })),
};
globalThis[Symbol.for("pi.host")] = host;

function respond(id, wireText) {
  post(`{"t":"res","id":${id},"ok":${wireText}}`);
}

function reject(id, error) {
  post(`{"t":"res","id":${id},"err":${wire.errorJson(error)}}`);
}

function importSpecifier(specifier) {
  if (path.isAbsolute(specifier)) return pathToFileURL(specifier).href;
  if (specifier.startsWith("./") || specifier.startsWith("../")) {
    return pathToFileURL(path.resolve(config.cwd, specifier)).href;
  }
  return specifier;
}

function defaultExport(namespace) {
  const value = namespace?.default;
  return value === undefined || value === null ? namespace : value;
}

/** A result to send as is, even when it is a promise. */
class Exact {
  constructor(value) {
    this.value = value;
  }
}

let portBaseline = 0;

function idle() {
  return new Promise((resolve) => {
    const check = () => {
      const active = process.getActiveResourcesInfo();
      const ports = active.filter((name) => name === "MessagePort").length;
      if (ports <= portBaseline && active.length === ports) resolve();
      else setTimeout(() => setImmediate(check), 5);
    };
    setImmediate(check);
  });
}

function run(request) {
  switch (request.op) {
    case "eval":
      return (0, eval)(request.source);
    case "import": {
      const loaded = import(importSpecifier(request.specifier));
      return loaded.then((namespace) => new Exact(request.default ? defaultExport(namespace) : namespace));
    }
    case "call": {
      const target = wire.lookup(request.target);
      const args = request.args.map(wire.decode);
      if (request.method === undefined) return target(...args);
      if (target === null || typeof target !== "object" && typeof target !== "function") {
        throw new TypeError("value is not an object");
      }
      const method = target[request.method];
      if (typeof method !== "function") throw new TypeError(`${request.method} is not a function`);
      return method.apply(target, args);
    }
    case "get": {
      const target = wire.lookup(request.target);
      if (target === null || typeof target !== "object" && typeof target !== "function") {
        throw new TypeError("value is not an object");
      }
      return target[request.key];
    }
    case "set": {
      const target = wire.lookup(request.target);
      if (target === null || typeof target !== "object" && typeof target !== "function") {
        throw new TypeError("value is not an object");
      }
      target[request.key] = wire.decode(request.value);
      return undefined;
    }
    case "json":
      return wire.lookup(request.target);
    case "idle":
      return idle();
    default:
      throw new Error(`unknown request ${request.op}`);
  }
}

function handle(request) {
  let result;
  try {
    result = run(request);
  } catch (error) {
    return reject(request.id, error);
  }
  const settle = (value) => {
    try {
      respond(request.id, wire.encodeAs(value instanceof Exact ? value.value : value, request.want));
    } catch (error) {
      reject(request.id, error);
    }
  };
  if (request.op !== "json" && result !== null && typeof result?.then === "function") {
    Promise.resolve(result).then(settle, (error) => reject(request.id, error));
  } else {
    settle(result);
  }
}

parentPort.on("message", (frame) => {
  switch (frame.t) {
    case "req":
      return handle(frame);
    case "settle": {
      const pending = pendingHost.get(frame.id);
      pendingHost.delete(frame.id);
      if (!pending) return;
      if (frame.err) return pending.reject(wire.toError(frame.err));
      try {
        pending.resolve(wire.decode(frame.ok));
      } catch (error) {
        pending.reject(error);
      }
      return;
    }
    case "release":
      return wire.release(frame.ids);
  }
});

function report(kind, error) {
  post(`{"t":"uncaught","kind":"${kind}","error":${wire.errorJson(error)}}`);
}

process.on("uncaughtException", (error) => {
  if (process.listenerCount("uncaughtException") === 1) report("exception", error);
});
process.on("unhandledRejection", (reason) => {
  if (process.listenerCount("unhandledRejection") === 1) report("rejection", reason);
});

// Node's own process.exit validates the code and emits `exit` first; the host hears the code only when
// the thread would end, and may end the whole process there.
const realReallyExit = process.reallyExit;
process.reallyExit = function reallyExit(code) {
  ask({ t: "exit", code: Number(code) | 0 });
  return realReallyExit.call(process, code);
};

process.argv = [process.execPath, ...config.argv.slice(1)];
for (const [name, stream] of [["stdout", process.stdout], ["stderr", process.stderr]]) {
  if (!config.tty?.[name]) continue;
  stream.isTTY = true;
  if (config.tty.columns) stream.columns = config.tty.columns;
  if (config.tty.rows) stream.rows = config.tty.rows;
}

require("./worker_modules.cjs").install({ ask, answer, hooksPort, config });
portBaseline = process.getActiveResourcesInfo().filter((name) => name === "MessagePort").length;
post('{"t":"ready"}');
