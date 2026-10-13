// node:process: an EventEmitter with Node's process fields, backed by Rust natives.
const EventEmitter = require("events");
const config = internal.config;
const process = new EventEmitter();
const nativeProcess = native.process;

const env = new Proxy(config.env, {
  set(target, key, value) {
    target[key] = String(value);
    return true;
  },
  defineProperty(target, key, descriptor) {
    if ("value" in descriptor) descriptor.value = String(descriptor.value);
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
process.abort = () => {
  throw internal.unsupported("process.abort");
};
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
  tickQueue.push([callback, args]);
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
process.cpuUsage = () => {
  throw internal.unsupported("process.cpuUsage");
};
process.resourceUsage = () => {
  throw internal.unsupported("process.resourceUsage");
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

// Standard streams are created on first use; stdin cannot be read inside pi.
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
  get() {
    if (!stdin) {
      const { Readable } = require("stream");
      stdin = new Readable({
        read() {
          this.destroy(internal.unsupported("Reading process.stdin"));
        },
      });
      stdin.fd = 0;
      stdin.isTTY = false;
      stdin.setRawMode = () => {
        throw internal.unsupported("process.stdin.setRawMode");
      };
    }
    return stdin;
  },
});

process[Symbol.toStringTag] = "process";
module.exports = process;
