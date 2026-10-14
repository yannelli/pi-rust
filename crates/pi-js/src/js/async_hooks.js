// node:async_hooks: AsyncLocalStorage and AsyncResource over the runtime's async context frame.
// A frame is an immutable Map from storage to value; `undefined` is the empty frame.
const { validate, invalidArgType, codeError } = require("internal/errors");
const context = internal.asyncContext;
const runInFrame = internal.runInFrame;
internal.enableAsyncContext();

let nextAsyncId = 1;
let executionId = 1;
let triggerId = 0;
let currentResource = {};
const hooks = new Set();

function withValue(frame, storage, value) {
  const next = new Map(frame);
  next.set(storage, value);
  return next;
}

function withoutValue(frame, storage) {
  if (frame === undefined || !frame.has(storage)) return frame;
  const next = new Map(frame);
  next.delete(storage);
  return next.size === 0 ? undefined : next;
}

function emitHook(name, ...args) {
  for (const hook of hooks) {
    const fn = hook[name];
    if (typeof fn === "function") fn.apply(hook, args);
  }
}

class AsyncLocalStorage {
  #defaultValue;
  #name;
  #enabled = false;
  constructor(options = {}) {
    validate.object(options, "options");
    this.#defaultValue = options.defaultValue;
    this.#name = options.name === undefined ? "" : String(options.name);
  }
  get name() {
    return this.#name;
  }
  static bind(fn) {
    return AsyncResource.bind(fn);
  }
  static snapshot() {
    const frame = context.get();
    return function runInAsyncScope(fn, ...args) {
      return runInFrame(frame, fn, this, args);
    };
  }
  disable() {
    if (!this.#enabled) return;
    this.#enabled = false;
    context.set(withoutValue(context.get(), this));
  }
  getStore() {
    const frame = context.get();
    if (frame !== undefined && frame.has(this)) return frame.get(this);
    return this.#enabled || this.#defaultValue === undefined ? undefined : this.#defaultValue;
  }
  enterWith(store) {
    this.#enabled = true;
    context.set(withValue(context.get(), this, store));
  }
  run(store, callback, ...args) {
    this.#enabled = true;
    return runInFrame(withValue(context.get(), this, store), callback, undefined, args);
  }
  exit(callback, ...args) {
    if (!this.#enabled) return Reflect.apply(callback, undefined, args);
    return runInFrame(withoutValue(context.get(), this), callback, undefined, args);
  }
}

class AsyncResource {
  #frame;
  #asyncId;
  #triggerAsyncId;
  constructor(type, options = {}) {
    validate.string(type, "type");
    if (typeof options === "number") options = { triggerAsyncId: options };
    const trigger = options.triggerAsyncId === undefined ? executionId : options.triggerAsyncId;
    if (!Number.isSafeInteger(trigger) || trigger < -1) {
      throw codeError(RangeError, "ERR_INVALID_ASYNC_ID", `Invalid triggerAsyncId value: ${trigger}`);
    }
    this.#frame = context.get();
    this.#asyncId = ++nextAsyncId;
    this.#triggerAsyncId = trigger;
    if (hooks.size > 0) emitHook("init", this.#asyncId, type, trigger, this);
  }
  runInAsyncScope(fn, thisArg, ...args) {
    const saved = [executionId, triggerId, currentResource];
    executionId = this.#asyncId;
    triggerId = this.#triggerAsyncId;
    currentResource = this;
    if (hooks.size > 0) emitHook("before", this.#asyncId);
    try {
      return runInFrame(this.#frame, fn, thisArg, args);
    } finally {
      if (hooks.size > 0) emitHook("after", this.#asyncId);
      [executionId, triggerId, currentResource] = saved;
    }
  }
  emitDestroy() {
    if (hooks.size > 0) emitHook("destroy", this.#asyncId);
    return this;
  }
  asyncId() {
    return this.#asyncId;
  }
  triggerAsyncId() {
    return this.#triggerAsyncId;
  }
  bind(fn, thisArg) {
    validate.function(fn, "fn");
    const resource = this;
    const bound = thisArg === undefined
      ? function (...args) { return resource.runInAsyncScope(fn, this, ...args); }
      : (...args) => resource.runInAsyncScope(fn, thisArg, ...args);
    Object.defineProperties(bound, {
      length: { configurable: true, value: fn.length },
      asyncResource: { configurable: true, enumerable: true, writable: true, value: this },
    });
    return bound;
  }
  static bind(fn, type, thisArg) {
    type = type || fn.name || "bound-anonymous-fn";
    return new AsyncResource(type).bind(fn, thisArg);
  }
}

// Hooks see AsyncResource objects only: QuickJS exposes no per-resource events for timers,
// promises, or I/O, so those run without init, before, after, or destroy calls.
class AsyncHook {
  constructor(callbacks) {
    for (const name of ["init", "before", "after", "destroy", "promiseResolve"]) {
      if (callbacks[name] !== undefined && typeof callbacks[name] !== "function") {
        throw codeError(TypeError, "ERR_ASYNC_CALLBACK", `hook.${name} must be a function`);
      }
      this[name] = callbacks[name];
    }
  }
  enable() {
    hooks.add(this);
    return this;
  }
  disable() {
    hooks.delete(this);
    return this;
  }
}

function createHook(callbacks) {
  if (callbacks === null || typeof callbacks !== "object") throw invalidArgType("callbacks", "of type object", callbacks);
  return new AsyncHook(callbacks);
}

const asyncWrapProviders = Object.freeze({
  __proto__: null, NONE: 0, DIRHANDLE: 1, DNSCHANNEL: 2, ELDHISTOGRAM: 3, FILEHANDLE: 4, FILEHANDLECLOSEREQ: 5,
  BLOBREADER: 6, FSEVENTWRAP: 7, FSREQCALLBACK: 8, FSREQPROMISE: 9, GETADDRINFOREQWRAP: 10, GETNAMEINFOREQWRAP: 11,
  HEAPSNAPSHOT: 12, HTTP2SESSION: 13, HTTP2STREAM: 14, HTTP2PING: 15, HTTP2SETTINGS: 16, HTTPINCOMINGMESSAGE: 17,
  HTTPCLIENTREQUEST: 18, JSSTREAM: 19, JSUDPWRAP: 20, MESSAGEPORT: 21, PIPECONNECTWRAP: 22, PIPESERVERWRAP: 23,
  PIPEWRAP: 24, PROCESSWRAP: 25, PROMISE: 26, QUERYWRAP: 27, SHUTDOWNWRAP: 28, SIGNALWRAP: 29, STATWATCHER: 30,
  STREAMPIPE: 31, TCPCONNECTWRAP: 32, TCPSERVERWRAP: 33, TCPWRAP: 34, TTYWRAP: 35, UDPSENDWRAP: 36, UDPWRAP: 37,
  SIGINTWATCHDOG: 38, WORKER: 39, WORKERHEAPSNAPSHOT: 40, WRITEWRAP: 41, ZLIB: 42,
});

module.exports = {
  AsyncLocalStorage,
  AsyncResource,
  createHook,
  executionAsyncId: () => executionId,
  triggerAsyncId: () => triggerId,
  executionAsyncResource: () => currentResource,
  asyncWrapProviders,
};
