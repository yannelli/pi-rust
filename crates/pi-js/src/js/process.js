// node:process: an EventEmitter with Node's process fields, backed by Rust natives.
const EventEmitter = require("events");
const config = internal.config;
const process = new EventEmitter();
const nativeProcess = native.process;

// With SHARE_ENV, `internal.envStore` names a native store that workers share.
const store = () => internal.envStore;
const nativeEnv = () => native.worker;
const env = new Proxy(config.env, {
  get(target, key, receiver) {
    if (store() === undefined || typeof key !== "string") return Reflect.get(target, key, receiver);
    return nativeEnv().envGet(store(), key) ?? undefined;
  },
  has(target, key) {
    if (store() === undefined || typeof key !== "string") return Reflect.has(target, key);
    return nativeEnv().envGet(store(), key) != null;
  },
  ownKeys(target) {
    return store() === undefined ? Reflect.ownKeys(target) : nativeEnv().envKeys(store()) || [];
  },
  getOwnPropertyDescriptor(target, key) {
    if (store() === undefined || typeof key !== "string") return Reflect.getOwnPropertyDescriptor(target, key);
    const value = nativeEnv().envGet(store(), key);
    return value == null ? undefined : { value, writable: true, enumerable: true, configurable: true };
  },
  deleteProperty(target, key) {
    if (store() !== undefined && typeof key === "string") nativeEnv().envDelete(store(), key);
    return Reflect.deleteProperty(target, key);
  },
  set(target, key, value) {
    if (store() !== undefined && typeof key === "string") nativeEnv().envSet(store(), key, String(value));
    target[key] = String(value);
    return true;
  },
  defineProperty(target, key, descriptor) {
    if ("value" in descriptor) descriptor.value = String(descriptor.value);
    if (store() !== undefined && typeof key === "string" && "value" in descriptor) nativeEnv().envSet(store(), key, descriptor.value);
    return Reflect.defineProperty(target, key, descriptor);
  },
});

const versions = { node: config.nodeVersion, pi_js: config.piJsVersion, quickjs: "quickjs-ng", uv: "0.0.0", v8: "0.0.0" };

Object.assign(process, {
  title: "pi",
  version: "v" + config.nodeVersion,
  versions,
  arch: config.arch,
  platform: config.platform,
  release: { name: "node", lts: "Jod" },
  argv: config.argv.slice(),
  argv0: config.argv[0] || "pi",
  execArgv: [],
  execPath: config.execPath,
  env,
  pid: config.pid,
  ppid: config.ppid,
  exitCode: undefined,
  features: { inspector: false, ipv6: true, tls: true, typescript: "transform" },
  config: { variables: {} },
  allowedNodeEnvironmentFlags: new Set(),
});

process.cwd = () => nativeProcess.cwd();
process.chdir = (directory) => nativeProcess.chdir(String(directory));
process.uptime = () => nativeProcess.uptime();
process.umask = () => 0o22;

process.exit = function exit(code) {
  if (code !== undefined) process.exitCode = code;
  const exitCode = process.exitCode === undefined ? 0 : Number(process.exitCode) | 0;
  if (!process._exiting) {
    process._exiting = true;
    process.emit("exit", exitCode);
  }
  nativeProcess.exit(exitCode);
  throw internal.unsupported("continuing after process.exit");
};
process.reallyExit = process.exit;
process.abort = () => native.process.abort();
process.kill = (pid, signal = "SIGTERM") => {
  let name = String(signal);
  if (typeof signal === "number") {
    const signals = require("os").constants.signals;
    name = Object.keys(signals).find((key) => signals[key] === signal) || name;
  }
  return nativeProcess.kill(Number(pid), name);
};

// The event loop calls runTicks before each microtask checkpoint, as Node does.
const tickQueue = [];
internal.runTicks = function runTicks() {
  while (tickQueue.length > 0) {
    const [callback, args] = tickQueue.shift();
    try {
      callback(...args);
    } catch (error) {
      if (tickQueue.length > 0) nativeProcess.scheduleTicks();
      throw error;
    }
  }
};
process.nextTick = function nextTick(callback, ...args) {
  if (typeof callback !== "function") {
    throw Object.assign(new TypeError('The "callback" argument must be of type function'), { code: "ERR_INVALID_ARG_TYPE" });
  }
  tickQueue.push([internal.bindContext(callback), args]);
  nativeProcess.scheduleTicks();
};

function hrtime(previous) {
  const [seconds, nanos] = nativeProcess.hrtime();
  if (!previous) return [seconds, nanos];
  let diffSeconds = seconds - previous[0];
  let diffNanos = nanos - previous[1];
  if (diffNanos < 0) {
    diffSeconds -= 1;
    diffNanos += 1e9;
  }
  return [diffSeconds, diffNanos];
}
hrtime.bigint = () => {
  const [seconds, nanos] = nativeProcess.hrtime();
  return BigInt(seconds) * 1000000000n + BigInt(nanos);
};
process.hrtime = hrtime;

function memoryUsage() {
  return { rss: nativeProcess.rss(), heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 };
}
memoryUsage.rss = () => nativeProcess.rss();
process.memoryUsage = memoryUsage;
process.cpuUsage = function cpuUsage(previous) {
  const [user, system] = native.os.resourceUsage();
  if (previous === undefined) return { user, system };
  const errors = require("internal/errors");
  if (previous === null || typeof previous !== "object") throw errors.invalidArgType("prevValue", "of type object", previous);
  for (const key of ["user", "system"]) {
    const value = previous[key];
    if (typeof value !== "number") throw errors.invalidArgType(`prevValue.${key}`, "of type number", value);
    if (!(value >= 0 && value <= Number.MAX_SAFE_INTEGER)) {
      throw errors.codeError(RangeError, "ERR_INVALID_ARG_VALUE", `The property 'prevValue.${key}' is invalid. Received ${value}`);
    }
  }
  return { user: user - previous.user, system: system - previous.system };
};
const RESOURCE_FIELDS = ["userCPUTime", "systemCPUTime", "maxRSS", "sharedMemorySize", "unsharedDataSize", "unsharedStackSize",
  "minorPageFault", "majorPageFault", "swappedOut", "fsRead", "fsWrite", "ipcSent", "ipcReceived", "signalsCount",
  "voluntaryContextSwitches", "involuntaryContextSwitches"];
process.resourceUsage = function resourceUsage() {
  const values = native.os.resourceUsage();
  return Object.fromEntries(RESOURCE_FIELDS.map((key, index) => [key, values[index]]));
};
for (const name of ["binding", "_linkedBinding", "dlopen", "setUncaughtExceptionCaptureCallback"]) {
  process[name] = () => {
    throw internal.unsupported(`process.${name}`);
  };
}
process.hasUncaughtExceptionCaptureCallback = () => false;
if (config.platform !== "win32") {
  for (const name of ["uid", "gid", "euid", "egid"]) {
    process[`get${name}`] = () => nativeProcess.uid(name);
  }
}

process.emitWarning = function emitWarning(warning, type, code) {
  if (type && typeof type === "object") {
    code = type.code;
    type = type.type;
  }
  if (typeof warning === "string") {
    const error = new Error(warning);
    error.name = type || "Warning";
    if (code) error.code = code;
    warning = error;
  }
  if (process.listenerCount("warning") > 0) {
    process.emit("warning", warning);
    return;
  }
  const prefix = warning.code ? `[${warning.code}] ` : "";
  native.io.writeText("stderr", `(node:${process.pid}) ${prefix}${warning.name}: ${warning.message}\n`);
};

// Standard streams are created on first use.
let stdout;
let stderr;
let stdin;
function outputStream(name, fd) {
  const { Writable } = require("stream");
  const stream = new Writable({
    write(chunk, encoding, callback) {
      if (typeof chunk === "string") native.io.writeText(name, chunk);
      else native.io.writeBytes(name, internal.bytesToLatin1(chunk));
      callback();
    },
    decodeStrings: false,
  });
  stream.fd = fd;
  stream._type = "pipe";
  stream._isStdio = true;
  Object.defineProperty(stream, "isTTY", { get: () => native.io.isTTY(name) });
  Object.defineProperty(stream, "columns", { get: () => (native.io.size() || [80, 24])[0] });
  Object.defineProperty(stream, "rows", { get: () => (native.io.size() || [80, 24])[1] });
  stream.getWindowSize = () => native.io.size() || [80, 24];
  stream.hasColors = (count = 16) => native.io.isTTY(name) && count <= 256;
  stream.getColorDepth = () => (native.io.isTTY(name) ? 8 : 1);
  stream.cursorTo = (x, y, callback) => require("readline").cursorTo(stream, x, y, callback);
  stream.moveCursor = (dx, dy, callback) => require("readline").moveCursor(stream, dx, dy, callback);
  stream.clearLine = (dir, callback) => require("readline").clearLine(stream, dir, callback);
  stream.clearScreenDown = (callback) => require("readline").clearScreenDown(stream, callback);
  stream.destroySoon = stream.destroy;
  stream._destroy = (error, callback) => callback(error);
  return stream;
}
Object.defineProperty(process, "stdout", { enumerable: true, configurable: true, get: () => (stdout ??= outputStream("stdout", 1)) });
Object.defineProperty(process, "stderr", { enumerable: true, configurable: true, get: () => (stderr ??= outputStream("stderr", 2)) });
Object.defineProperty(process, "stdin", {
  enumerable: true,
  configurable: true,
  get: () => (stdin ??= inputStream()),
});

// Reading starts on first read and keeps the process alive only while the stream flows.
function inputStream() {
  const { Readable } = require("stream");
  const { Buffer } = require("buffer");
  let id = null;
  let ended = false;
  const close = () => {
    if (id !== null) native.pipe.close(id);
    id = null;
  };
  const stream = new Readable({
    read() {
      if (ended) return;
      id ??= native.pipe.stdin((kind, value) => {
        if (kind === "data") stream.push(Buffer.from(value.buffer, value.byteOffset, value.byteLength));
        else if (kind === "end") {
          ended = true;
          close();
          stream.push(null);
        } else if (kind === "error") stream.destroy(value);
      });
      native.pipe.ref(id, true);
    },
    destroy(error, callback) {
      ended = true;
      close();
      callback(error);
    },
  });
  stream.on("pause", () => id !== null && native.pipe.ref(id, false));
  stream.fd = 0;
  if (native.pipe.stdinIsTTY()) {
    stream.isTTY = true;
    stream.isRaw = false;
    stream.setRawMode = function setRawMode(flag) {
      native.pipe.setRawMode(!!flag);
      if (flag && !this.isRaw) process.once("exit", () => this.isRaw && native.pipe.setRawMode(false));
      this.isRaw = !!flag;
      return this;
    };
  }
  return stream;
}

// A forked child talks to its parent over the channel named in PI_JS_IPC.
if (config.env.PI_JS_IPC) {
  const path = config.env.PI_JS_IPC;
  const serialization = config.env.PI_JS_IPC_SERIALIZATION || "json";
  delete config.env.PI_JS_IPC;
  delete config.env.PI_JS_IPC_SERIALIZATION;
  require("internal/ipc").setupChannel(process, (callback) => native.pipe.connect(path, callback), serialization);
}

process[Symbol.toStringTag] = "process";
module.exports = process;
