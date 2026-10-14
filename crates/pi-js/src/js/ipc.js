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

const channelClosed = () => errors.codeError(Error, "ERR_IPC_CHANNEL_CLOSED", "Channel closed");

// Give `target` (a ChildProcess or process) Node's send, disconnect, connected, and channel.
function setupChannel(target, open, serialization = "json") {
  const advanced = serialization === "advanced";
  const decoder = new TextDecoder();
  let pending = "";
  let refs = 0;
  let explicitRef = null;
  let writes = Promise.resolve();
  const id = open((kind, value) => {
    if (kind === "connect") target._channelConnected = true;
    else if (kind === "data") {
      pending += decoder.decode(value, { stream: true });
      let newline;
      while ((newline = pending.indexOf("\n")) !== -1) {
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (!line) continue;
        const parsed = JSON.parse(line);
        const message = advanced ? decodeAdvanced(parsed) : parsed;
        const internalMessage = message !== null && typeof message === "object" && typeof message.cmd === "string" && message.cmd.startsWith("NODE_");
        target.emit(internalMessage ? "internalMessage" : "message", message, undefined);
      }
    } else if (kind === "end" || kind === "error") {
      if (target.connected) finishDisconnect();
      else nativePipe.close(id);
    }
  });
  const updateRef = () => nativePipe.ref(id, explicitRef ?? refs > 0);
  const counted = (name) => name === "message" || name === "disconnect";
  target.on("newListener", (name) => {
    if (counted(name)) {
      refs++;
      updateRef();
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
  function finishDisconnect() {
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
    if (target.connected) finishDisconnect();
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
    if (message === undefined) throw errors.missingArgs("message");
    if (!["string", "object", "number", "boolean"].includes(typeof message)) {
      throw errors.invalidArgType("message", "one of type string, object, number, or boolean", message);
    }
    if (handle !== undefined && handle !== null) throw errors.codeError(TypeError, "ERR_INVALID_HANDLE_TYPE", "This handle type cannot be sent");
    const line = JSON.stringify(advanced ? encodeAdvanced(message) : message) + "\n";
    const bytes = internal.bytesToLatin1(Buffer.from(line));
    writes = writes.then(() => nativePipe.write(id, bytes)).then(
      () => typeof callback === "function" && process.nextTick(callback, null),
      (error) => (typeof callback === "function" ? process.nextTick(callback, error) : target.emit("error", error)),
    );
    return true;
  };
  target.disconnect = function disconnect() {
    if (!target.connected) {
      target.emit("error", errors.codeError(Error, "ERR_IPC_DISCONNECTED", "IPC channel is already disconnected"));
      return;
    }
    target.connected = false;
    writes.then(() => {
      target.connected = true;
      finishDisconnect();
    });
  };
  return id;
}

module.exports = { setupChannel, encodeAdvanced, decodeAdvanced };
