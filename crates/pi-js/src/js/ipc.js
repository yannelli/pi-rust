// The IPC channel of child_process.fork: newline-delimited JSON ("json" serialization) or a
// tagged JSON form of the structured clone algorithm ("advanced"), on a native pipe.
const errors = require("internal/errors");
const { Buffer } = require("buffer");
const nativePipe = native.pipe;

const TYPED = { Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array, Float32Array, Float64Array, BigInt64Array, BigUint64Array, DataView, Buffer };

function encodeAdvanced(value, seen = new Map()) {
  if (value === undefined) return { $: "u" };
  if (typeof value === "number") return Number.isFinite(value) && !Object.is(value, -0) ? value : { $: "n", v: String(value) };
  if (typeof value === "bigint") return { $: "b", v: value.toString() };
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "function" || typeof value === "symbol") {
    throw new (globalThis.DOMException || Error)(`${String(value)} could not be cloned.`, "DataCloneError");
  }
  if (seen.has(value)) return { $: "r", i: seen.get(value) };
  const index = seen.size;
  seen.set(value, index);
  const encode = (item) => encodeAdvanced(item, seen);
  if (Array.isArray(value)) return { $: "a", i: index, v: Array.from(value, encode) };
  if (value instanceof Date) return { $: "d", i: index, v: value.getTime() };
  if (value instanceof RegExp) return { $: "x", i: index, v: [value.source, value.flags] };
  if (value instanceof Map) return { $: "m", i: index, v: [...value].map(([key, item]) => [encode(key), encode(item)]) };
  if (value instanceof Set) return { $: "s", i: index, v: [...value].map(encode) };
  if (value instanceof Error) return { $: "e", i: index, v: [value.name, value.message, value.stack, encode(value.cause)] };
  if (value instanceof ArrayBuffer) return { $: "ab", i: index, v: Buffer.from(value).toString("base64") };
  if (ArrayBuffer.isView(value)) {
    const type = Buffer.isBuffer(value) ? "Buffer" : value.constructor.name;
    return { $: "t", i: index, t: type, v: Buffer.from(value.buffer, value.byteOffset, value.byteLength).toString("base64") };
  }
  return { $: "o", i: index, v: Object.keys(value).map((key) => [key, encode(value[key])]) };
}

function decodeAdvanced(value, refs = []) {
  if (value === null || typeof value !== "object") return value;
  const decode = (item) => decodeAdvanced(item, refs);
  const keep = (result) => ((refs[value.i] = result), result);
  switch (value.$) {
    case "u": return undefined;
    case "n": return Number(value.v);
    case "b": return BigInt(value.v);
    case "r": return refs[value.i];
    case "a": {
      const list = keep([]);
      for (const item of value.v) list.push(decode(item));
      return list;
    }
    case "d": return keep(new Date(value.v));
    case "x": return keep(new RegExp(value.v[0], value.v[1]));
    case "m": {
      const map = keep(new Map());
      for (const [key, item] of value.v) map.set(decode(key), decode(item));
      return map;
    }
    case "s": {
      const set = keep(new Set());
      for (const item of value.v) set.add(decode(item));
      return set;
    }
    case "e": {
      const Ctor = globalThis[value.v[0]] && globalThis[value.v[0]].prototype instanceof Error ? globalThis[value.v[0]] : Error;
      const error = keep(new Ctor(value.v[1]));
      error.stack = value.v[2];
      const cause = decode(value.v[3]);
      if (cause !== undefined) error.cause = cause;
      return error;
    }
    case "ab": {
      const bytes = Buffer.from(value.v, "base64");
      return keep(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength));
    }
    case "t": {
      const bytes = Buffer.from(value.v, "base64");
      if (value.t === "Buffer") return keep(bytes);
      const buffer = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
      const Ctor = TYPED[value.t] || Uint8Array;
      return keep(Ctor === DataView ? new DataView(buffer) : new Ctor(buffer));
    }
    default: {
      const object = keep({});
      for (const [key, item] of value.v) object[key] = decode(item);
      return object;
    }
  }
}

// Handles (net.Socket, net.Server) travel as {cmd: "NODE_HANDLE", type, msg}: the descriptor
// rides along with SCM_RIGHTS on Unix, or as WSADuplicateSocket bytes ("wsa") on Windows.

// A forked child loads this module while `process` is still being built, so check the platform on use.
const windows = () => process.platform === "win32";
const MAX_HANDLE_RETRANSMISSIONS = 3;

function handleType(handle) {
  const net = require("net");
  if (handle instanceof net.Socket && handle._handle !== null) return "net.Socket";
  if (handle instanceof net.Server && handle._handle !== null) return "net.Server";
  throw errors.codeError(TypeError, "ERR_INVALID_HANDLE_TYPE", "This handle type cannot be sent");
}

function adoptHandle(type, id) {
  const net = require("net");
  if (type === "net.Socket") {
    const { kAccepted } = require("internal/net_socket");
    return new net.Socket({ [kAccepted]: native.net.adoptSocket(id) });
  }
  if (type === "net.Server") return net.createServer()._adoptHandle(id);
  native.net.closeHandle(id);
  return undefined;
}

// What `internalMessage` listeners get with a NODE_HANDLE message: a received descriptor that only
// supports `close()`, where Node passes its raw TCP handle.
class ReceivedHandle {
  #id;
  constructor(id) {
    this.#id = id;
  }
  static take(handle) {
    const id = handle instanceof ReceivedHandle ? handle.#id : undefined;
    if (id !== undefined) handle.#id = undefined;
    return id;
  }
  close() {
    const id = ReceivedHandle.take(this);
    if (id !== undefined) native.net.closeHandle(id);
  }
}

const channelClosed = () => errors.codeError(Error, "ERR_IPC_CHANNEL_CLOSED", "Channel closed");
const isInternal = (message) =>
  message !== null && typeof message === "object" && typeof message.cmd === "string" && message.cmd.startsWith("NODE_");
const isProtocolReply = (message) =>
  message !== null && typeof message === "object" && (message.cmd === "NODE_HANDLE_ACK" || message.cmd === "NODE_HANDLE_NACK");

// Give `target` (a ChildProcess or process) Node's send, disconnect, connected, and channel. As in Node, a sent
// handle holds back every later message until the peer acknowledges it, and a socket is resent on a NACK.
function setupChannel(target, open, serialization = "json", peerPid = () => 0) {
  const advanced = serialization === "advanced";
  const encode = (message) => (advanced ? encodeAdvanced(message) : message);
  const decoder = new TextDecoder();
  let pending = "";
  let refs = 0;
  let explicitRef = null;
  let writes = Promise.resolve();
  let channelOpen = true;
  let inFlight = null;
  const received = [];
  target._handleQueue = null;
  target._pendingMessage = null;
  const deliver = (message, handle) => {
    if (isInternal(message)) target.emit("internalMessage", message, handle);
    else if (held) held.push([message, handle]);
    else target.emit("message", message, handle);
  };
  const id = open((kind, value, handles) => {
    if (kind === "connect") target._channelConnected = true;
    else if (kind === "data") {
      if (handles && handles.length) received.push(...handles);
      pending += decoder.decode(value, { stream: true });
      let newline;
      while ((newline = pending.indexOf("\n")) !== -1) {
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (!line) continue;
        const parsed = JSON.parse(line);
        if (parsed !== null && typeof parsed === "object" && parsed.cmd === "NODE_HANDLE" && typeof parsed.type === "string") {
          const handleId = windows()
            ? parsed.wsa === undefined ? undefined : native.net.importHandle(parsed.wsa)
            : received.shift();
          target.emit("internalMessage", parsed, handleId === undefined ? undefined : new ReceivedHandle(handleId));
        } else {
          deliver(advanced ? decodeAdvanced(parsed) : parsed, undefined);
        }
      }
    } else if (kind === "end" || kind === "error") {
      if (channelOpen) finishDisconnect();
      else nativePipe.close(id);
    }
  });
  // Node reads the channel only after the main script has run, so its listeners are in place. Here data can
  // arrive first: hold messages until the first 'message' listener exists.
  let held = target.listenerCount("message") > 0 ? null : [];
  const updateRef = () => nativePipe.ref(id, explicitRef ?? refs > 0);
  const counted = (name) => name === "message" || name === "disconnect";
  target.on("newListener", (name) => {
    if (counted(name)) {
      refs++;
      updateRef();
    }
    if (name === "message" && held) {
      const messages = held;
      held = null;
      process.nextTick(() => messages.forEach(([message, handle]) => target.emit("message", message, handle)));
    }
  });
  target.on("removeListener", (name) => {
    if (counted(name)) {
      refs--;
      updateRef();
    }
  });
  refs = target.listenerCount("message") + target.listenerCount("disconnect");
  updateRef();

  // Protocol replies must not turn a peer's disconnect into a user error.
  const reply = (cmd) => channelOpen && target._send({ cmd }, null, { swallowErrors: true });
  // The built-in protocol listener, removable like Node's: ACK/NACK settle the pending handle, NODE_HANDLE adopts.
  target.on("internalMessage", (message, handle) => {
    if (isProtocolReply(message)) return settleHandle(message.cmd === "NODE_HANDLE_ACK");
    if (message.cmd !== "NODE_HANDLE") return;
    const handleId = ReceivedHandle.take(handle);
    if (handleId === undefined) return reply("NODE_HANDLE_NACK");
    reply("NODE_HANDLE_ACK");
    const adopted = adoptHandle(message.type, handleId);
    deliver(advanced ? decodeAdvanced(message.msg) : message.msg, adopted);
  });

  function closePending() {
    native.net.closeHandle(target._pendingMessage.copy);
    target._pendingMessage = null;
  }
  function releaseQueue() {
    const queue = target._handleQueue;
    if (!queue) return;
    target._handleQueue = null;
    inFlight = null;
    if (target._pendingMessage) resend(target._pendingMessage);
    for (const entry of queue) {
      // As in Node, a queued handle that was closed meanwhile goes out as its message alone.
      const handle = entry.handle && entry.handle._handle === null ? null : entry.handle;
      target._send(entry.message, handle, entry.options, entry.callback);
    }
    if (!target.connected && channelOpen && !target._handleQueue) writes.then(finishDisconnect);
  }
  function settleHandle(ack) {
    const sent = target._pendingMessage;
    if (sent && ack) closePending();
    else if (sent && sent.retransmissions++ === MAX_HANDLE_RETRANSMISSIONS) {
      closePending();
      process.emitWarning("Handle did not reach the receiving process correctly", "SentHandleNotReceivedWarning");
    }
    releaseQueue();
  }
  // A failed write drops the copy at once, as Node closes the handle, and lets held-back messages go.
  function failHandle(entry) {
    if (inFlight !== entry) return;
    if (target._pendingMessage === entry) closePending();
    releaseQueue();
  }
  // Errors of a send go to its callback, or to 'error' unless swallowed, on a later tick as in Node.
  function fail(error, options, callback) {
    if (options.swallowErrors) return;
    if (typeof callback === "function") process.nextTick(callback, error);
    else process.nextTick(() => target.emit("error", error));
  }
  function queueWrite(write, options, callback, onFailure) {
    writes = writes.then(write).then(
      () => typeof callback === "function" && process.nextTick(callback, null),
      (error) => {
        if (onFailure) onFailure();
        fail(error, options, callback);
      },
    );
  }
  // The whole wrapper is serialized before any socket copy exists, so a message that cannot be serialized (or whose
  // toJSON drops it) leaves nothing to release; "wsa" is appended without serializing the message again.
  const handleJson = (type, message) => JSON.stringify({ cmd: "NODE_HANDLE", type, msg: encode(message) });
  function writeHandle(entry, json, sent, wsa) {
    const line = wsa === undefined ? json : `${json.slice(0, -1)},"wsa":${JSON.stringify(wsa)}}`;
    const bytes = internal.bytesToLatin1(Buffer.from(line + "\n"));
    target._handleQueue = [];
    inFlight = entry;
    const write = windows() ? () => nativePipe.write(id, bytes) : () => nativePipe.writeHandle(id, bytes, sent);
    // A write that never ran left `sent` registered; closing an id already taken or duplicated does nothing.
    queueWrite(write, entry.options, entry.callback, () => {
      native.net.closeHandle(sent);
      failHandle(entry);
    });
  }
  // As in Node, a resend serializes the message again. A failure to prepare it releases every copy and reports the
  // error; releaseQueue then sends the held-back messages.
  function resend(entry) {
    let json, sent, wsa;
    try {
      json = handleJson(entry.type, entry.message);
      sent = native.net.cloneHandle(entry.copy);
      if (windows()) wsa = native.net.duplicateHandle(sent, peerPid());
    } catch (error) {
      if (sent !== undefined) native.net.closeHandle(sent);
      closePending();
      fail(error, entry.options, entry.callback);
      return;
    }
    writeHandle(entry, json, sent, wsa);
  }
  function finishDisconnect() {
    if (!channelOpen) return;
    channelOpen = false;
    if (target._pendingMessage) closePending();
    target._handleQueue = null;
    inFlight = null;
    target.connected = false;
    target.channel = undefined;
    nativePipe.close(id);
    process.nextTick(() => target.emit("disconnect"));
  }
  target.connected = true;
  target.channel = {
    ref() {
      explicitRef = true;
      updateRef();
    },
    unref() {
      explicitRef = false;
      updateRef();
    },
    hasRef: () => explicitRef ?? refs > 0,
  };
  target._closeChannel = () => {
    if (channelOpen) finishDisconnect();
  };
  target.send = function send(message, handle, options, callback) {
    if (typeof handle === "function") [callback, handle, options] = [handle, undefined, undefined];
    else if (typeof options === "function") [callback, options] = [options, undefined];
    else if (options !== undefined) errors.validate.object(options, "options");
    if (!target.connected) {
      const error = channelClosed();
      if (typeof callback === "function") process.nextTick(callback, error);
      else process.nextTick(() => target.emit("error", error));
      return false;
    }
    return target._send(message, handle, { swallowErrors: false, ...options }, callback);
  };
  target._send = function _send(message, handle, options, callback) {
    if (message === undefined) throw errors.missingArgs("message");
    if (!["string", "object", "number", "boolean"].includes(typeof message)) {
      throw errors.invalidArgType("message", "one of type string, object, number, or boolean", message);
    }
    if (typeof options === "boolean") options = { swallowErrors: options };
    options = { swallowErrors: false, ...options };
    const type = handle === undefined || handle === null ? null : handleType(handle);
    if (target._handleQueue && (type || !isProtocolReply(message))) {
      target._handleQueue.push({ message, handle: type ? handle : null, options, callback });
      return target._handleQueue.length === 1;
    }
    if (!type) {
      const bytes = internal.bytesToLatin1(Buffer.from(JSON.stringify(encode(message)) + "\n"));
      queueWrite(() => nativePipe.write(id, bytes), options, callback);
      return true;
    }
    const json = handleJson(type, message);
    const keep = type === "net.Socket" && !options.keepOpen;
    const entry = { message, type, options, callback, retransmissions: 0, copy: undefined };
    let sent, wsa;
    try {
      sent = native.net.exportHandle(handle._handle, false);
      if (keep) entry.copy = native.net.exportHandle(handle._handle, false);
      if (windows()) wsa = native.net.duplicateHandle(sent, peerPid());
    } catch (error) {
      for (const copy of [sent, entry.copy]) if (copy !== undefined) native.net.closeHandle(copy);
      throw error;
    }
    // Only now is the socket shared: its close must leave the FIN and unread bytes to the peer.
    native.net.markExported(handle._handle);
    if (keep) {
      handle.destroy();
      target._pendingMessage = entry;
    }
    writeHandle(entry, json, sent, wsa);
    return true;
  };
  target.disconnect = function disconnect() {
    if (!target.connected) {
      target.emit("error", errors.codeError(Error, "ERR_IPC_DISCONNECTED", "IPC channel is already disconnected"));
      return;
    }
    target.connected = false;
    if (!target._handleQueue) writes.then(finishDisconnect);
  };
  return id;
}

module.exports = { setupChannel, encodeAdvanced, decodeAdvanced };
