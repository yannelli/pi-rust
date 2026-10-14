// node:wasi: the WASI class for WebAssembly modules, with the preview1 (and "unstable") system
// calls implemented in JavaScript over the fs module (see wasi_calls.js).
const { codeError, invalidArgType, invalidArgValue, validate } = require("internal/errors");
const { createCalls } = require("internal/wasi_calls");
const { Memory } = require("internal/wasi_fds");

const kExitCode = Symbol("kExitCode");
const kSetMemory = Symbol("kSetMemory");
const kStarted = Symbol("kStarted");
const kInstance = Symbol("kInstance");
const kBindingName = Symbol("kBindingName");

process.emitWarning("WASI is an experimental feature and might change at any time", "ExperimentalWarning");

function validateUndefined(value, name) {
  if (value !== undefined) throw invalidArgType(name, "undefined", value);
}

class WASI {
  constructor(options = {}) {
    validate.object(options, "options");
    validate.string(options.version, "options.version");
    const bindings = { unstable: "wasi_unstable", preview1: "wasi_snapshot_preview1" };
    if (!Object.hasOwn(bindings, options.version)) {
      throw invalidArgValue("options.version", options.version, "unsupported WASI version");
    }
    this[kBindingName] = bindings[options.version];
    if (options.args !== undefined) validate.array(options.args, "options.args");
    const args = (options.args || []).map(String);
    const env = [];
    if (options.env !== undefined) {
      validate.object(options.env, "options.env");
      for (const [key, value] of Object.entries(options.env)) if (value !== undefined) env.push(`${key}=${value}`);
    }
    const preopens = [];
    if (options.preopens !== undefined) {
      validate.object(options.preopens, "options.preopens");
      for (const [key, value] of Object.entries(options.preopens)) preopens.push([String(key), String(value)]);
    }
    const { stdin = 0, stdout = 1, stderr = 2 } = options;
    validate.integer(stdin, "options.stdin", 0, 2147483647);
    validate.integer(stdout, "options.stdout", 0, 2147483647);
    validate.integer(stderr, "options.stderr", 0, 2147483647);
    let returnOnExit = true;
    if (options.returnOnExit !== undefined) {
      validate.boolean(options.returnOnExit, "options.returnOnExit");
      returnOnExit = options.returnOnExit;
    }
    const memory = new Memory();
    const calls = createCalls({ args, env, preopens, stdio: [stdin, stdout, stderr] }, memory);
    // proc_exit throws a value Wasm cannot catch, and start() turns it into the exit code.
    calls.proc_exit = (code) => {
      if (!returnOnExit) process.reallyExit(code >>> 0);
      this[kExitCode] = code >>> 0;
      throw kExitCode;
    };
    this[kSetMemory] = (value) => {
      if (!(value instanceof WebAssembly.Memory)) {
        const message = '"instance.exports.memory" property must be a WebAssembly.Memory object';
        throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", message);
      }
      memory.memory = value;
    };
    this.wasiImport = calls;
    this[kStarted] = false;
    this[kExitCode] = 0;
    this[kInstance] = undefined;
  }

  #finalizeBindings(instance) {
    if (this[kStarted]) throw codeError(Error, "ERR_WASI_ALREADY_STARTED", "WASI instance has already started");
    validate.object(instance, "instance");
    validate.object(instance.exports, "instance.exports");
    this[kSetMemory](instance.exports.memory);
    this[kInstance] = instance;
    this[kStarted] = true;
  }

  // Must export _start and not _initialize.
  start(instance) {
    this.#finalizeBindings(instance);
    const { _start, _initialize } = this[kInstance].exports;
    validate.function(_start, "instance.exports._start");
    validateUndefined(_initialize, "instance.exports._initialize");
    try {
      _start();
    } catch (error) {
      if (error !== kExitCode) throw error;
    }
    return this[kExitCode];
  }

  // Must not export _start; may export _initialize.
  initialize(instance) {
    this.#finalizeBindings(instance);
    const { _start, _initialize } = this[kInstance].exports;
    validateUndefined(_start, "instance.exports._start");
    if (_initialize !== undefined) {
      validate.function(_initialize, "instance.exports._initialize");
      _initialize();
    }
  }

  getImportObject() {
    return { [this[kBindingName]]: this.wasiImport };
  }
}

module.exports = { WASI };
