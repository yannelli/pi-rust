// node:worker_threads. A Worker is a new pi-js runtime on its own OS thread. It talks to its
// parent over two native port pairs: one public (postMessage) and one for control messages.
const EventEmitter = require("events");
const path = require("path");
const { codeError, invalidArgType, validate } = require("internal/errors");
const ports = require("internal/worker_ports");
const nativeWorker = native.worker;

const SHARE_ENV = Symbol.for("nodejs.worker_threads.SHARE_ENV");
const info = internal.workerInfo;
const isMainThread = !info;
const threadId = info ? info.threadId : 0;
const environmentData = new Map();
let parentPort = null;
let workerData = null;

function setEnvironmentData(key, value) {
  if (value === undefined) environmentData.delete(key);
  else environmentData.set(key, value);
}

function getEnvironmentData(key) {
  return environmentData.get(key);
}

function resourceLimits(options) {
  const limits = { maxYoungGenerationSizeMb: 0, maxOldGenerationSizeMb: 0, codeRangeSizeMb: 0, stackSizeMb: 4 };
  if (options === undefined) return limits;
  validate.object(options, "options.resourceLimits");
  for (const key of Object.keys(limits)) {
    if (options[key] === undefined) continue;
    if (typeof options[key] !== "number") throw invalidArgType(`options.resourceLimits.${key}`, "of type number", options[key]);
    limits[key] = options[key];
  }
  return limits;
}

function scriptFile(filename) {
  if (filename !== null && typeof filename === "object" && typeof filename.href === "string") {
    const url = new URL(filename.href);
    if (url.protocol !== "file:") throw codeError(TypeError, "ERR_INVALID_URL_SCHEME", "The URL must be of scheme file");
    return require("url").fileURLToPath(url);
  }
  if (typeof filename !== "string") throw invalidArgType("filename", "of type string or an instance of URL", filename);
  if (filename.startsWith("file:")) return require("url").fileURLToPath(filename);
  if (!path.isAbsolute(filename) && !/^\.\.?[\\/]/.test(filename)) {
    throw codeError(TypeError, "ERR_WORKER_PATH", "The worker script or module filename must be an absolute path or a " +
      `relative path starting with './' or '../'. Received "${filename}"`);
  }
  return path.resolve(filename);
}

const kHandle = Symbol("kHandle");
const kPort = Symbol("kPort");
const kControl = Symbol("kControl");
const kOnEvent = Symbol("kOnEvent");

class Worker extends EventEmitter {
  constructor(filename, options = {}) {
    super();
    validate.object(options, "options");
    const isEval = !!options.eval;
    if (isEval && typeof filename !== "string") throw invalidArgType("filename", "of type string", filename);
    const file = isEval ? null : scriptFile(filename);
    const argv = options.argv === undefined ? [] : (validate.array(options.argv, "options.argv"), options.argv.map(String));
    let execArgv = [];
    if (options.execArgv !== undefined) {
      validate.array(options.execArgv, "options.execArgv");
      execArgv = options.execArgv.map(String);
    }
    let env = { ...process.env };
    let envStore;
    if (options.env === SHARE_ENV) {
      internal.envStore ??= nativeWorker.envCreate({ ...process.env });
      envStore = internal.envStore;
    } else if (options.env !== undefined && options.env !== null) {
      validate.object(options.env, "options.env");
      env = Object.fromEntries(Object.entries(options.env).map(([key, value]) => [key, String(value)]));
    }
    for (const name of ["stdin", "stdout", "stderr"]) {
      if (options[name] !== undefined) validate.boolean(options[name], `options.${name}`);
    }
    const name = options.name === undefined ? "" : (validate.string(options.name, "options.name"), options.name);
    this.resourceLimits = resourceLimits(options.resourceLimits);
    const [portParent, portChild] = nativeWorker.channel();
    const [controlParent, controlChild] = nativeWorker.channel();
    const port = (this[kPort] = ports.createPort(portParent));
    const control = (this[kControl] = ports.createPort(controlParent));
    try {
      control.postMessage({ workerData: options.workerData, environmentData: [...environmentData] }, options.transferList);
    } catch (error) {
      port.close();
      control.close();
      nativeWorker.close(portChild);
      nativeWorker.close(controlChild);
      throw error;
    }
    this.threadId = nativeWorker.nextThreadId();
    const workerInfo = { threadId: this.threadId, portId: portChild, controlId: controlChild, eval: isEval,
      code: isEval ? filename : null, filename: file, execArgv, stdin: !!options.stdin, envStore, name,
      resourceLimits: this.resourceLimits };
    const init = `__pi_internal.workerInfo = ${JSON.stringify(workerInfo)}; __pi_internal.builtin("worker_threads");` +
      " __pi_internal.workerStart(); void 0";
    const { Readable, Writable } = require("stream");
    this.stdout = new Readable({ read() {} });
    this.stderr = new Readable({ read() {} });
    this.stdin = options.stdin ? new Writable({
      write(chunk, encoding, callback) {
        control.postMessage({ type: "stdin", chunk: Buffer.from(chunk, encoding) });
        callback();
      },
      final(callback) {
        control.postMessage({ type: "stdinEnd" });
        callback();
      },
    }) : null;
    const limits = this.resourceLimits;
    const heap = limits.maxOldGenerationSizeMb + limits.maxYoungGenerationSizeMb;
    this[kHandle] = nativeWorker.spawn({
      name: `pi-js-worker-${this.threadId}`,
      argv: [process.execPath, ...(file ? [file] : []), ...argv],
      env,
      init,
      stdout: !!options.stdout,
      stderr: !!options.stderr,
      memoryLimit: heap > 0 ? Math.ceil(heap * 1024 * 1024) : null,
      stackSize: limits.stackSizeMb > 0 ? Math.ceil(limits.stackSizeMb * 1024 * 1024) : null,
    }, (kind, value) => this[kOnEvent](kind, value));
    port.on("message", (value) => this.emit("message", value));
    port.on("messageerror", (error) => this.emit("messageerror", error));
    port.unref();
    control.on("message", (message) => message && message.type === "error" && this.emit("error", message.error));
    control.unref();
    this.performance = { eventLoopUtilization: () => ({ idle: 0, active: 0, utilization: 0 }) };
  }
  [kOnEvent](kind, value) {
    if (kind === "online") this.emit("online");
    else if (kind === "stdout" || kind === "stderr") this[kind].push(Buffer.from(value));
    else if (kind === "error") this.emit("error", value);
    else if (kind === "exit") {
      this[kHandle] = null;
      this[kPort].close();
      this[kControl].close();
      this.stdout.push(null);
      this.stderr.push(null);
      this.threadId = -1;
      this.emit("exit", value);
    }
  }
  postMessage(...args) {
    if (this[kHandle] !== null) this[kPort].postMessage(...args);
  }
  terminate(callback) {
    if (typeof callback === "function") {
      process.emitWarning("Passing a callback to worker.terminate() is deprecated. It returns a Promise instead.",
        "DeprecationWarning", "DEP0132");
      this.once("exit", (code) => process.nextTick(callback, null, code));
    }
    if (this[kHandle] === null) return Promise.resolve();
    return new Promise((resolve) => {
      this.once("exit", resolve);
      nativeWorker.terminate(this[kHandle]);
    });
  }
  ref() {
    if (this[kHandle] !== null) nativeWorker.ref(this[kHandle], true);
  }
  unref() {
    if (this[kHandle] !== null) nativeWorker.ref(this[kHandle], false);
  }
  getHeapSnapshot() {
    return Promise.reject(internal.unsupported("worker.getHeapSnapshot"));
  }
}

// Inside a worker: read the start-up message, then set up parentPort, stdin, and error reports.
if (info) {
  const control = ports.createPort(info.controlId);
  const start = ports.receiveMessageOnPort(control);
  const payload = start ? start.message : {};
  workerData = payload.workerData ?? null;
  for (const [key, value] of payload.environmentData || []) environmentData.set(key, value);
  parentPort = ports.createPort(info.portId);
  if (info.envStore !== undefined && info.envStore !== null) internal.envStore = info.envStore;
  process.execArgv = info.execArgv;
  process.chdir = () => {
    throw codeError(TypeError, "ERR_WORKER_UNSUPPORTED_OPERATION", "process.chdir() is not supported in workers");
  };
  const { Readable } = require("stream");
  const stdin = new Readable({ read() {} });
  if (!info.stdin) stdin.push(null);
  Object.defineProperty(process, "stdin", { value: stdin, configurable: true, enumerable: true, writable: true });
  control.on("message", (message) => {
    if (message && message.type === "stdin") stdin.push(Buffer.from(message.chunk));
    else if (message && message.type === "stdinEnd") stdin.push(null);
  });
  control.unref();
  internal.workerUncaught = (error) => {
    if (process._exiting) return true;
    try {
      control.postMessage({ type: "error", error });
    } catch {
      control.postMessage({ type: "error", error: new Error(String(error)) });
    }
    process.exit(1);
    return true;
  };
  internal.workerStart = () => {
    if (info.eval) {
      const filename = path.join(process.cwd(), "[worker eval]");
      const module = { exports: {}, filename, id: filename };
      try {
        const run = new Function("exports", "require", "module", "__filename", "__dirname", info.code);
        run.call(module.exports, module.exports, internal.createRequire(filename), module, filename, process.cwd());
      } catch (error) {
        internal.workerUncaught(error);
      }
      return;
    }
    import(info.filename).catch((error) => internal.workerUncaught(error));
  };
}

module.exports = {
  isMainThread,
  isInternalThread: false,
  parentPort,
  threadId,
  workerData,
  resourceLimits: info ? info.resourceLimits : {},
  Worker,
  MessageChannel: ports.MessageChannel,
  MessagePort: ports.MessagePort,
  BroadcastChannel: ports.BroadcastChannel,
  receiveMessageOnPort: ports.receiveMessageOnPort,
  markAsUntransferable: ports.markAsUntransferable,
  isMarkedAsUntransferable: ports.isMarkedAsUntransferable,
  moveMessagePortToContext: ports.moveMessagePortToContext,
  setEnvironmentData,
  getEnvironmentData,
  SHARE_ENV,
};
