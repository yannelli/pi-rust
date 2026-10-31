// node:child_process on Rust process natives. With the default output, 'inherit' passes the real
// descriptors. With an embedder's output sink, inherited stdout and stderr go to the sink and
// inherited stdin is empty, because the embedder owns the terminal.
const EventEmitter = require("events");
const { Buffer } = require("buffer");
const path = require("path");
const { Readable, Writable } = require("stream");
const errors = require("internal/errors");
const nativeChild = native.child;
const platform = internal.config.platform;
const kCustomPromisify = Symbol.for("nodejs.util.promisify.custom");

function invalidArg(name, expected, value) {
  return Object.assign(new TypeError(`The "${name}" argument must be ${expected}. Received ${value === null ? "null" : typeof value}`), { code: "ERR_INVALID_ARG_TYPE" });
}

function signalName(number) {
  if (number === null || number === undefined) return null;
  const signals = require("os").constants.signals;
  return Object.keys(signals).find((name) => signals[name] === number) || `SIG${number}`;
}

// Node's stdio entries as native ones: a kind, an `fs.open` descriptor ({ file }), or a TCP socket
// ({ socket }) the child gets a copy of. Descriptors 0 to 2 are this process's own stdio.
function normalizeStdio(stdio, sync) {
  let list;
  if (stdio === undefined || stdio === null) list = ["pipe", "pipe", "pipe"];
  else if (typeof stdio === "string") {
    if (!["ignore", "pipe", "overlapped", "inherit"].includes(stdio)) throw errors.invalidArgValue("stdio", stdio);
    list = [stdio, stdio, stdio];
  } else if (Array.isArray(stdio)) list = Array.from(stdio);
  else throw invalidArg("options.stdio", "of type string or Array", stdio);
  while (list.length < 3) list.push(undefined);
  return list.map((entry, index) => {
    entry ??= index < 3 ? "pipe" : "ignore";
    if (entry === "overlapped" || (typeof entry === "number" && entry < 0)) entry = "pipe";
    if (entry === "ipc") return "ignore";
    if (entry === "ignore" || ((entry === "pipe" || entry === "inherit") && index < 3)) return entry;
    if (entry === "inherit") return "ignore";
    if (entry === "pipe") {
      if (platform === "win32") throw internal.unsupported(`child_process stdio[${index}] 'pipe' on Windows`);
      return entry;
    }
    const own = entry !== null && typeof entry === "object" ? [process.stdin, process.stdout, process.stderr].indexOf(entry) : -1;
    if (own >= 0) entry = own;
    const fd = typeof entry === "number" ? entry : entry !== null && typeof entry === "object" && typeof entry.fd === "number" ? entry.fd : undefined;
    if (fd !== undefined) {
      if (fd > 2) return { file: fd };
      if (index < 3) return "inherit";
      throw internal.unsupported(`child_process stdio[${index}] descriptor ${fd}`);
    }
    const net = require("net");
    if ((entry instanceof net.Socket || entry instanceof net.Server) && typeof entry._handle === "number") {
      return { socket: entry._handle, stream: entry };
    }
    if (ArrayBuffer.isView(entry) || typeof entry === "string") {
      if (sync) return "pipe";
      throw errors.codeError(TypeError, "ERR_INVALID_SYNC_FORK_INPUT", `Asynchronous forks do not support Buffer, TypedArray, DataView or string input: ${require("util").inspect(entry)}`);
    }
    throw errors.invalidArgValue("stdio", entry);
  });
}

function resolveCwd(cwd) {
  if (cwd === undefined || cwd === null) return process.cwd();
  if (cwd instanceof URL) return require("url").fileURLToPath(cwd);
  return path.resolve(String(cwd));
}

// Without real descriptors to pass, inherited stdin is empty and inherited output is piped here
// and forwarded to the process streams.
function nativeStdio(entry, index) {
  if (entry.socket !== undefined) return { socket: entry.socket };
  if (entry !== "inherit" || internal.config.processStdio) return entry;
  return index === 0 ? "ignore" : "pipe";
}

// Command, arguments, and options for the native spawn, including the shell wrapper.
function normalizeSpawn(file, args, options, sync = false) {
  if (typeof file !== "string" || file === "") throw invalidArg("file", "of type string", file);
  if (args && !Array.isArray(args)) [options, args] = [args, []];
  args = (args || []).map(String);
  options = { ...options };
  const env = options.env || process.env;
  const stdio = normalizeStdio(options.stdio, sync);
  let command = file;
  let argv = args;
  let verbatim = !!options.windowsVerbatimArguments;
  if (options.shell) {
    const line = [file, ...args].join(" ");
    if (platform === "win32") {
      command = typeof options.shell === "string" ? options.shell : env.ComSpec || "cmd.exe";
      argv = /^(?:.*\\)?cmd(?:\.exe)?$/i.test(command) ? ["/d", "/s", "/c", `"${line}"`] : ["-c", line];
      verbatim = true;
    } else {
      command = typeof options.shell === "string" ? options.shell : "/bin/sh";
      argv = ["-c", line];
    }
  }
  if (options.argv0 !== undefined && typeof options.argv0 !== "string") throw invalidArg("options.argv0", "of type string", options.argv0);
  const ipcCount = Array.isArray(options.stdio) ? options.stdio.filter((entry) => entry === "ipc").length : 0;
  if (ipcCount > 1) throw Object.assign(new Error("Child process can have only one IPC pipe"), { code: "ERR_IPC_ONE_PIPE" });
  const serialization = options.serialization ?? "json";
  if (serialization !== "json" && serialization !== "advanced") {
    throw Object.assign(new TypeError(`The property 'options.serialization' must be one of: 'json', 'advanced'. Received ${require("util").inspect(serialization)}`), { code: "ERR_INVALID_ARG_VALUE" });
  }
  // The pi binary runs JavaScript like node when it spawns itself, as Node code expects.
  const extraEnv = [];
  if (!options.shell && path.resolve(file) === process.execPath) extraEnv.push(["PI_JS_FORK", "1"]);
  return {
    ipc: ipcCount === 1,
    serialization,
    native: {
      file: command,
      args: argv,
      cwd: resolveCwd(options.cwd),
      env: [...Object.keys(env).filter((key) => env[key] !== undefined).map((key) => [key, String(env[key])]), ...extraEnv],
      stdio: stdio.map(nativeStdio),
      windowsHide: !!options.windowsHide,
      windowsVerbatimArguments: verbatim,
      detached: !!options.detached,
      argv0: options.argv0,
      uid: options.uid,
      gid: options.gid,
    },
    stdio,
    file,
    spawnargs: [options.argv0 || file, ...args],
    options,
  };
}

class ChildProcess extends EventEmitter {
  constructor() {
    super();
    this.pid = undefined;
    this.exitCode = null;
    this.signalCode = null;
    this.killed = false;
    this.connected = false;
    this.stdin = null;
    this.stdout = null;
    this.stderr = null;
    this.stdio = [null, null, null];
    this._id = null;
    this._pending = 0;
  }
  spawn(spec) {
    this.spawnfile = spec.file;
    this.spawnargs = spec.spawnargs;
    const outputs = {};
    for (const [index, name] of [[1, "stdout"], [2, "stderr"]]) {
      if (spec.native.stdio[index] !== "pipe") continue;
      const readable = new Readable({
        read() {},
        destroy: (error, callback) => {
          if (this._id !== null) nativeChild.closeOutput(this._id, name);
          callback(error);
        },
      });
      if (spec.stdio[index] === "inherit") readable.on("data", (chunk) => process[name].write(chunk));
      else this[name] = readable;
      outputs[name] = readable;
      this._pending++;
      readable.on("close", () => this._streamClosed());
    }
    this.stdio = spec.stdio.map(() => null);
    [this.stdio[1], this.stdio[2]] = [this.stdout, this.stderr];
    if (spec.ipc) {
      const ipc = require("internal/ipc");
      this._channelId = ipc.setupChannel(this, (callback) => {
        const listening = native.pipe.listen(callback);
        spec.native.env.push(["PI_JS_IPC", listening.path], ["PI_JS_IPC_SERIALIZATION", spec.serialization]);
        return listening.id;
      }, spec.serialization, () => this.pid);
      this._pending++;
      this.once("disconnect", () => this._streamClosed());
    }
    const result = nativeChild.spawn(spec.native, (kind, value, signal) => this._event(kind, value, signal, outputs));
    if (result.error) {
      if (this._closeChannel) this._closeChannel();
      Object.assign(result.error, { spawnargs: spec.spawnargs.slice(1) });
      process.nextTick(() => {
        this.emit("error", result.error);
        this.emit("close", -2, null);
      });
      for (const stream of Object.values(outputs)) stream.destroy();
      return result.error;
    }
    this._id = result.id;
    this.pid = result.pid;
    this._attachStdio(spec, result.pipes, outputs);
    if (spec.native.stdio[0] === "pipe") {
      this.stdin = new Writable({
        write: (chunk, encoding, callback) => {
          nativeChild.write(this._id, internal.bytesToLatin1(chunk)).then(() => callback(), callback);
        },
        final: (callback) => {
          nativeChild.end(this._id).then(() => callback(), callback);
        },
      });
      // A child that exits before reading its input must not crash the extension host (EPIPE).
      this.stdin.on("error", () => {});
      this.stdio[0] = this.stdin;
    }
    process.nextTick(() => this.emit("spawn"));
    return undefined;
  }
  // Extra pipes become sockets that 'close' waits for; a socket handed to the child stops reading.
  _attachStdio(spec, pipes, outputs) {
    const { Socket, kAccepted } = require("internal/net_socket");
    for (const [index, id, info] of pipes) {
      const socket = new Socket({ [kAccepted]: { id, info } });
      this.stdio[index] = socket;
      outputs[`fd${index}`] = socket;
      this._pending++;
      socket.on("close", () => this._streamClosed());
    }
    for (const entry of spec.stdio) {
      if (!entry.stream || entry.stream instanceof require("net").Server) continue;
      entry.stream.pause();
      entry.stream._paused = true;
      native.net.pause(entry.socket);
    }
  }
  _event(kind, value, signal, outputs) {
    if (kind === "stdout" || kind === "stderr") {
      outputs[kind].push(Buffer.from(value.buffer, value.byteOffset, value.byteLength));
      return;
    }
    if (kind === "stdout-end" || kind === "stderr-end") {
      outputs[kind.slice(0, 6)].push(null);
      return;
    }
    this.exitCode = signal === null || signal === undefined ? value : null;
    this.signalCode = signalName(signal);
    if (this.stdin && !this.stdin.destroyed) this.stdin.destroy();
    // Drain outputs nobody reads so they can end and 'close' can fire (Node's flushStdio).
    for (const stream of Object.values(outputs)) {
      const unused = stream.listenerCount("data") === 0 && stream.listenerCount("readable") === 0;
      if (unused && stream._readableState.pipes.length === 0) stream.resume();
    }
    this.emit("exit", this.exitCode, this.signalCode);
    this._exited = true;
    // A child that never connected its channel will not close it either.
    if (this.connected && this._closeChannel && !this._channelConnected) this._closeChannel();
    this._maybeClose();
  }
  _streamClosed() {
    this._pending--;
    this._maybeClose();
  }
  _maybeClose() {
    if (this._exited && this._pending === 0 && !this._closed) {
      this._closed = true;
      // Let 'end' listeners on the output streams run before 'close', as in Node.
      setImmediate(() => this.emit("close", this.exitCode, this.signalCode));
    }
  }
  kill(signal = "SIGTERM") {
    const signals = require("os").constants.signals;
    const name = signal === 0 ? "SIG0" : typeof signal === "number" ? signalName(signal) : typeof signal === "string" ? signal.toUpperCase() : undefined;
    if (signal !== 0 && !signals[name]) {
      throw errors.codeError(TypeError, "ERR_UNKNOWN_SIGNAL", `Unknown signal: ${signal}`);
    }
    if (this._id === null || this.exitCode !== null || this.signalCode !== null) return false;
    let sent;
    try {
      sent = nativeChild.kill(this._id, name);
    } catch (error) {
      if (error.code === "ESRCH") return false;
      throw error;
    }
    if (sent) this.killed = true;
    return sent;
  }
  ref() {
    if (this._id !== null) nativeChild.ref(this._id, true);
  }
  unref() {
    if (this._id !== null) nativeChild.ref(this._id, false);
  }
  [Symbol.dispose]() {
    if (!this.killed) this.kill();
  }
}

function spawn(file, args, options) {
  const spec = normalizeSpawn(file, args, options);
  const child = new ChildProcess();
  child.spawn(spec);
  if (spec.options.signal) {
    const onabort = () => {
      child.kill(spec.options.killSignal || "SIGTERM");
      child.emit("error", Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR", cause: spec.options.signal.reason }));
    };
    if (spec.options.signal.aborted) process.nextTick(onabort);
    else spec.options.signal.addEventListener("abort", onabort, { once: true });
  }
  if (spec.options.timeout > 0) {
    const timer = setTimeout(() => child.kill(spec.options.killSignal || "SIGTERM"), spec.options.timeout);
    child.once("exit", () => clearTimeout(timer));
  }
  return child;
}

function fork(modulePath, args, options) {
  if (modulePath instanceof URL) modulePath = require("url").fileURLToPath(modulePath);
  if (typeof modulePath !== "string") throw invalidArg("modulePath", "of type string or an instance of URL", modulePath);
  if (args === undefined || args === null) args = [];
  else if (typeof args === "object" && !Array.isArray(args)) [options, args] = [args, []];
  else if (!Array.isArray(args)) throw invalidArg("args", "an instance of Array", args);
  options = { ...options };
  const execArgv = options.execArgv || process.execArgv;
  if (typeof options.stdio === "string") options.stdio = [options.stdio, options.stdio, options.stdio, "ipc"];
  else if (!Array.isArray(options.stdio)) options.stdio = options.silent ? ["pipe", "pipe", "pipe", "ipc"] : ["inherit", "inherit", "inherit", "ipc"];
  else if (!options.stdio.includes("ipc")) {
    throw Object.assign(new Error("Forked processes must have an IPC channel, missing value 'ipc' in options.stdio"), { code: "ERR_CHILD_PROCESS_IPC_REQUIRED" });
  }
  const execPath = options.execPath || process.execPath;
  const child = spawn(execPath, [...execArgv, modulePath, ...args.map(String)], { ...options, shell: false });
  return child;
}

function execFile(file, args, options, callback) {
  if (typeof args === "function") [callback, args, options] = [args, [], {}];
  else if (!Array.isArray(args) && args !== null && args !== undefined) [callback, options, args] = [options, args, []];
  if (typeof options === "function") [callback, options] = [options, {}];
  options = { encoding: "utf8", timeout: 0, maxBuffer: 1024 * 1024, killSignal: "SIGTERM", ...options };
  const child = spawn(file, args || [], { ...options, timeout: 0 });
  const command = [file, ...(args || [])].join(" ");
  const output = { stdout: [], stderr: [] };
  let size = 0;
  let failure = null;
  let killedByUs = false;
  for (const name of ["stdout", "stderr"]) {
    if (!child[name]) continue;
    child[name].on("data", (chunk) => {
      size += chunk.length;
      if (size > options.maxBuffer && !failure) {
        failure = Object.assign(new RangeError(`${name} maxBuffer length exceeded`), { code: "ERR_CHILD_PROCESS_STDIO_MAXBUFFER" });
        kill();
      }
      output[name].push(chunk);
    });
  }
  // Like Node: stop reading before the kill, so a grandchild holding the pipes cannot delay 'close'.
  const kill = () => {
    killedByUs = true;
    if (child.stdout) child.stdout.destroy();
    if (child.stderr) child.stderr.destroy();
    child.kill(options.killSignal);
  };
  let timer = null;
  if (options.timeout > 0) timer = setTimeout(kill, options.timeout);
  const decode = (chunks) => {
    const buffer = Buffer.concat(chunks);
    return options.encoding && options.encoding !== "buffer" && Buffer.isEncoding(options.encoding) ? buffer.toString(options.encoding) : buffer;
  };
  const finish = (code, signal) => {
    if (timer) clearTimeout(timer);
    const stdout = decode(output.stdout);
    const stderr = decode(output.stderr);
    let error = failure;
    if (!error && (code !== 0 || signal)) {
      error = new Error(`Command failed: ${command}\n${stderr}`);
      Object.assign(error, { code: code === null ? undefined : code, killed: killedByUs || child.killed, signal, cmd: command });
    }
    if (error) Object.assign(error, { stdout, stderr });
    if (typeof callback === "function") callback(error, stdout, stderr);
  };
  child.on("close", finish);
  child.on("error", (error) => {
    if (timer) clearTimeout(timer);
    if (typeof callback === "function") {
      const fail = callback;
      callback = null;
      fail(Object.assign(error, { cmd: command }), decode(output.stdout), decode(output.stderr));
    }
  });
  return child;
}

function exec(command, options, callback) {
  if (typeof options === "function") [callback, options] = [options, {}];
  return execFile(command, [], { ...options, shell: (options && options.shell) || true }, callback);
}

function promisified(run) {
  return (...args) => {
    let child;
    const promise = new Promise((resolve, reject) => {
      child = run(...args, (error, stdout, stderr) => (error ? reject(Object.assign(error, { stdout, stderr })) : resolve({ stdout, stderr })));
    });
    promise.child = child;
    return promise;
  };
}
exec[kCustomPromisify] = promisified(exec);
execFile[kCustomPromisify] = promisified(execFile);

function spawnSync(file, args, options) {
  const spec = normalizeSpawn(file, args, options, true);
  options = spec.options;
  let input = options.input;
  if (typeof input === "string") input = Buffer.from(input, options.encoding && options.encoding !== "buffer" ? options.encoding : "utf8");
  const raw = nativeChild.spawnSync(spec.native, input === undefined ? undefined : internal.bytesToLatin1(input), options.timeout, options.killSignal || "SIGTERM");
  const decode = (bytes, index) => {
    if (bytes === undefined) return null;
    const buffer = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    if (spec.stdio[index] === "inherit") {
      process[index === 1 ? "stdout" : "stderr"].write(buffer);
      return null;
    }
    return options.encoding && options.encoding !== "buffer" ? buffer.toString(options.encoding) : buffer;
  };
  const stdout = decode(raw.stdout, 1);
  const stderr = decode(raw.stderr, 2);
  const output = spec.stdio.map((entry, index) => (index === 1 ? stdout : index === 2 ? stderr : index > 2 && raw.extra && raw.extra[index] ? decode(raw.extra[index], index) : null));
  const result = { pid: raw.pid || 0, output, stdout, stderr, status: raw.status ?? null, signal: signalName(raw.signal) };
  if (raw.error) result.error = Object.assign(raw.error, { spawnargs: spec.spawnargs.slice(1) });
  else if (raw.timedOut) result.error = Object.assign(new Error(`spawnSync ${spec.file} ETIMEDOUT`), { code: "ETIMEDOUT", syscall: `spawnSync ${spec.file}`, path: spec.file, spawnargs: spec.spawnargs.slice(1) });
  const limit = options.maxBuffer === undefined ? 1024 * 1024 : options.maxBuffer;
  if (!result.error && [stdout, stderr].some((item) => item && item.length > limit)) {
    result.error = Object.assign(new Error(`spawnSync ${spec.file} ENOBUFS`), { code: "ENOBUFS", syscall: `spawnSync ${spec.file}`, path: spec.file });
  }
  return result;
}

function checkSync(result, command) {
  if (result.error) throw Object.assign(result.error, result);
  if (result.status !== 0 || result.signal) {
    const stderr = result.stderr ? result.stderr.toString() : "";
    const error = new Error(`Command failed: ${command}${stderr ? `\n${stderr}` : ""}`);
    throw Object.assign(error, result);
  }
  return result.stdout;
}

function execFileSync(file, args, options) {
  if (!Array.isArray(args)) [options, args] = [args, []];
  options = { stdio: ["pipe", "pipe", "inherit"], ...options };
  return checkSync(spawnSync(file, args, options), [file, ...args].join(" "));
}

function execSync(command, options) {
  options = { stdio: ["pipe", "pipe", "inherit"], ...options, shell: (options && options.shell) || true };
  return checkSync(spawnSync(command, [], options), command);
}

module.exports = {
  ChildProcess,
  spawn,
  exec,
  execFile,
  spawnSync,
  execSync,
  execFileSync,
  fork,
  _forkChild() {
    throw internal.unsupported("child_process._forkChild");
  },
};
