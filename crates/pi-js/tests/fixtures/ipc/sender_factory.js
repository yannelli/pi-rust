// Loads the shipped ipc.js (`SOURCE`) with real pi-js internals and fake natives that only record which socket ids
// are open. `plan.platform` is a mock-platform proof of the JS branches, not of WinSock or the kernel.
const internal = globalThis.__pi_internal;
const EventEmitter = internal.builtin("events");
const net = internal.builtin("net");
const injected = (code = "EINJECTED") => Object.assign(new Error(`injected ${code}`), { code });

// `plan` is a list of `writeHandle` steps, or { writes, exports, clones, dups, platform }. A write step "ok" takes the
// sent id, "before" throws without taking it, "after" takes it and rejects; other steps fail with that code.
function channel(plan = {}) {
  const steps = Array.isArray(plan) ? { writes: [...plan] } : plan;
  const script = (name) => [...(steps[name] || [])];
  const writes = script("writes");
  const exports = script("exports");
  const clones = script("clones");
  const dups = script("dups");
  let next = 100;
  const ids = new Map();
  const lines = [];
  const counts = { pipeCloses: 0, destroyed: 0 };
  const calls = { exports: 0, clones: 0, marks: 0, unmarkedExports: 0 };
  const fresh = () => {
    ids.set(next, "open");
    return next++;
  };
  const take = (id) => {
    if (ids.get(id) !== "open") throw new Error(`id ${id} is ${ids.get(id)}`);
    ids.set(id, "taken");
  };
  const step = (list) => {
    const code = list.shift();
    if (code && code !== "ok") throw injected(code);
  };
  const record = (bytes) => {
    const message = JSON.parse(bytes.trim());
    if (!message || message.cmd !== "NODE_HANDLE") return lines.push(message);
    lines.push(message.wsa === undefined ? { handle: message.msg } : { handle: message.msg, wsa: message.wsa });
  };
  const native = {
    pipe: {
      write: (_pipe, bytes) => (record(bytes), Promise.resolve()),
      writeHandle: (_pipe, bytes, sent) => {
        const code = writes.shift() || "ok";
        if (code === "before") throw injected();
        take(sent);
        record(bytes);
        return code === "ok" ? Promise.resolve() : Promise.reject(injected());
      },
      close: () => counts.pipeCloses++,
      ref: () => {},
    },
    net: {
      exportHandle: (_socket, mark) => {
        calls.exports++;
        if (mark !== false) calls.unmarkedExports++;
        step(exports);
        return fresh();
      },
      cloneHandle: (id) => {
        calls.clones++;
        if (ids.get(id) !== "open") throw new Error(`clone of id ${id}, which is ${ids.get(id)}`);
        step(clones);
        return fresh();
      },
      // As in handles.rs, a duplicate takes its id first and then may fail.
      duplicateHandle: (id) => {
        take(id);
        step(dups);
        return `wsa-${id}`;
      },
      closeHandle: (id) => ids.get(id) === "open" && ids.set(id, "closed"),
      markExported: () => (calls.marks++, true),
    },
  };
  // Unless a case asks for "win32", the Unix branch runs on every host.
  const platform = steps.platform || "linux";
  const lexicalProcess = {
    platform,
    nextTick: process.nextTick.bind(process),
    emitWarning: process.emitWarning.bind(process),
  };
  const module = { exports: {} };
  const parameters = ["module", "exports", "require", "native", "internal", "process"];
  const factory = new Function(...parameters, `"use strict"; ${SOURCE}`);
  factory.call(module.exports, module, module.exports, internal.builtin, native, internal, lexicalProcess);
  const target = new EventEmitter();
  let reader;
  module.exports.setupChannel(target, (callback) => ((reader = callback), 1), "json", () => 4242);
  const socket = Object.create(net.Socket.prototype);
  socket._handle = 7;
  socket.destroy = () => {
    counts.destroyed++;
    socket._handle = null;
    return socket;
  };
  const feed = (message) => reader("data", new TextEncoder().encode(JSON.stringify(message) + "\n"), []);
  const open = () => [...ids].filter(([, state]) => state === "open").map(([id]) => id);
  const state = () => ({
    lines,
    ...counts,
    connected: target.connected,
    pending: target._pendingMessage,
    queue: target._handleQueue,
    open: open(),
  });
  // Like `state`, but safe for messages JSON cannot hold, with the native call counts.
  const summary = () => ({
    lines,
    ...counts,
    ...calls,
    connected: target.connected,
    pending: target._pendingMessage !== null,
    queued: target._handleQueue === null ? null : target._handleQueue.length,
    open: open(),
  });
  return { target, socket, feed, state, summary, calls };
}

// A send callback that records error codes; `at(n)` resolves once it has been called `n` times.
function recorder() {
  const calls = [];
  const waiters = [];
  const callback = (error) => {
    calls.push(error ? error.code : null);
    for (const waiter of waiters.filter((entry) => calls.length >= entry.count)) waiter.resolve(calls);
  };
  const at = (count) =>
    calls.length >= count ? Promise.resolve(calls) : new Promise((resolve) => waiters.push({ count, resolve }));
  return { callback, at, calls };
}
