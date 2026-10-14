// node:events: EventEmitter with Node's listener, once, error, and async helper semantics.
const kCapture = Symbol("kCapture");
const errorMonitor = Symbol("events.errorMonitor");
let defaultMaxListeners = 10;

function checkListener(listener) {
  if (typeof listener !== "function") {
    const error = new TypeError(`The "listener" argument must be of type function. Received ${listener === null ? "null" : typeof listener}`);
    error.code = "ERR_INVALID_ARG_TYPE";
    throw error;
  }
}

function EventEmitter(options) {
  EventEmitter.init.call(this, options);
}

EventEmitter.init = function init(options) {
  if (this._events === undefined || this._events === Object.getPrototypeOf(this)._events) {
    this._events = Object.create(null);
    this._eventsCount = 0;
  }
  this._maxListeners = this._maxListeners || undefined;
  if (options && options.captureRejections) this[kCapture] = true;
};

EventEmitter.prototype._events = undefined;
EventEmitter.prototype._eventsCount = 0;
EventEmitter.prototype._maxListeners = undefined;

EventEmitter.prototype.setMaxListeners = function setMaxListeners(n) {
  if (typeof n !== "number" || n < 0 || Number.isNaN(n)) throw new RangeError(`The value of "n" is out of range. Received ${n}`);
  this._maxListeners = n;
  return this;
};

EventEmitter.prototype.getMaxListeners = function getMaxListeners() {
  return this._maxListeners === undefined ? defaultMaxListeners : this._maxListeners;
};

EventEmitter.prototype.emit = function emit(type, ...args) {
  const events = this._events;
  if (type === "error" && events !== undefined && events[errorMonitor] !== undefined) {
    this.emit(errorMonitor, ...args);
  }
  const handler = events === undefined ? undefined : events[type];
  if (handler === undefined) {
    if (type === "error") {
      const error = args[0];
      if (error instanceof Error) throw error;
      const wrapped = new Error("Unhandled error." + (error === undefined ? "" : ` (${String(error)})`));
      wrapped.code = "ERR_UNHANDLED_ERROR";
      wrapped.context = error;
      throw wrapped;
    }
    return false;
  }
  if (typeof handler === "function") {
    callListener(this, handler, args);
  } else {
    for (const listener of handler.slice()) callListener(this, listener, args);
  }
  return true;
};

function callListener(emitter, listener, args) {
  const result = Reflect.apply(listener, emitter, args);
  if (emitter[kCapture] && result && typeof result.then === "function") {
    result.then(undefined, (error) => process.nextTick(() => emitter.emit("error", error)));
  }
}

function addListener(target, type, listener, prepend) {
  checkListener(listener);
  let events = target._events;
  if (events === undefined) {
    events = target._events = Object.create(null);
    target._eventsCount = 0;
  } else if (events.newListener !== undefined) {
    target.emit("newListener", type, listener.listener ? listener.listener : listener);
    events = target._events;
  }
  const existing = events[type];
  if (existing === undefined) {
    events[type] = listener;
    ++target._eventsCount;
  } else if (typeof existing === "function") {
    events[type] = prepend ? [listener, existing] : [existing, listener];
  } else if (prepend) {
    existing.unshift(listener);
  } else {
    existing.push(listener);
  }
  const list = events[type];
  const max = target.getMaxListeners();
  if (Array.isArray(list) && max > 0 && list.length > max && !list.warned) {
    list.warned = true;
    const warning = new Error(`Possible EventEmitter memory leak detected. ${list.length} ${String(type)} listeners added. Use emitter.setMaxListeners() to increase limit`);
    warning.name = "MaxListenersExceededWarning";
    if (globalThis.process && typeof process.emitWarning === "function") process.emitWarning(warning);
  }
  return target;
}

EventEmitter.prototype.addListener = function on(type, listener) {
  return addListener(this, type, listener, false);
};
EventEmitter.prototype.on = EventEmitter.prototype.addListener;
EventEmitter.prototype.prependListener = function prependListener(type, listener) {
  return addListener(this, type, listener, true);
};

function onceWrapper(state) {
  return function wrapped(...args) {
    if (!state.fired) {
      state.target.removeListener(state.type, state.wrapFn);
      state.fired = true;
      return state.listener.apply(state.target, args);
    }
    return undefined;
  };
}

function wrapOnce(target, type, listener) {
  const state = { fired: false, wrapFn: undefined, target, type, listener };
  const wrapped = onceWrapper(state);
  wrapped.listener = listener;
  state.wrapFn = wrapped;
  return wrapped;
}

EventEmitter.prototype.once = function once(type, listener) {
  checkListener(listener);
  return this.on(type, wrapOnce(this, type, listener));
};
EventEmitter.prototype.prependOnceListener = function prependOnceListener(type, listener) {
  checkListener(listener);
  return this.prependListener(type, wrapOnce(this, type, listener));
};

EventEmitter.prototype.removeListener = function removeListener(type, listener) {
  checkListener(listener);
  const events = this._events;
  if (events === undefined) return this;
  const list = events[type];
  if (list === undefined) return this;
  if (list === listener || list.listener === listener) {
    if (--this._eventsCount === 0) this._events = Object.create(null);
    else delete events[type];
    if (events.removeListener) this.emit("removeListener", type, list.listener || listener);
  } else if (typeof list !== "function") {
    let position = -1;
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i] === listener || list[i].listener === listener) {
        position = i;
        break;
      }
    }
    if (position < 0) return this;
    list.splice(position, 1);
    if (list.length === 1) events[type] = list[0];
    if (events.removeListener !== undefined) this.emit("removeListener", type, listener);
  }
  return this;
};
EventEmitter.prototype.off = EventEmitter.prototype.removeListener;

EventEmitter.prototype.removeAllListeners = function removeAllListeners(type) {
  const events = this._events;
  if (events === undefined) return this;
  if (events.removeListener === undefined) {
    if (arguments.length === 0) {
      this._events = Object.create(null);
      this._eventsCount = 0;
    } else if (events[type] !== undefined) {
      if (--this._eventsCount === 0) this._events = Object.create(null);
      else delete events[type];
    }
    return this;
  }
  if (arguments.length === 0) {
    for (const key of Reflect.ownKeys(events)) {
      if (key !== "removeListener") this.removeAllListeners(key);
    }
    this.removeAllListeners("removeListener");
    this._events = Object.create(null);
    this._eventsCount = 0;
    return this;
  }
  const listeners = events[type];
  if (typeof listeners === "function") this.removeListener(type, listeners);
  else if (listeners !== undefined) for (let i = listeners.length - 1; i >= 0; i--) this.removeListener(type, listeners[i]);
  return this;
};

function listenersOf(target, type, unwrap) {
  const events = target._events;
  if (events === undefined || events[type] === undefined) return [];
  const list = events[type];
  if (typeof list === "function") return unwrap ? [list.listener || list] : [list];
  return unwrap ? list.map((listener) => listener.listener || listener) : list.slice();
}

EventEmitter.prototype.listeners = function listeners(type) {
  return listenersOf(this, type, true);
};
EventEmitter.prototype.rawListeners = function rawListeners(type) {
  return listenersOf(this, type, false);
};
EventEmitter.prototype.listenerCount = function listenerCount(type, listener) {
  const list = this._events === undefined ? undefined : this._events[type];
  if (list === undefined) return 0;
  if (typeof list === "function") return listener === undefined || list === listener || list.listener === listener ? 1 : 0;
  if (listener === undefined) return list.length;
  return list.filter((item) => item === listener || item.listener === listener).length;
};
EventEmitter.prototype.eventNames = function eventNames() {
  return this._eventsCount > 0 ? Reflect.ownKeys(this._events) : [];
};

function abortError(signal) {
  const error = new Error("The operation was aborted");
  error.name = "AbortError";
  error.code = "ABORT_ERR";
  if (signal && signal.reason !== undefined) error.cause = signal.reason;
  return error;
}

function once(emitter, name, options = {}) {
  const signal = options.signal;
  if (signal && signal.aborted) return Promise.reject(abortError(signal));
  return new Promise((resolve, reject) => {
    const listener = (...args) => {
      cleanup();
      resolve(args);
    };
    const onError = (error) => {
      cleanup();
      reject(error);
    };
    const onAbort = () => {
      cleanup();
      reject(abortError(signal));
    };
    function cleanup() {
      remove(emitter, name, listener);
      if (name !== "error") remove(emitter, "error", onError);
      if (signal) signal.removeEventListener("abort", onAbort);
    }
    add(emitter, name, listener, true);
    if (name !== "error") add(emitter, "error", onError, true);
    if (signal) signal.addEventListener("abort", onAbort, { once: true });
  });
}

function add(emitter, name, listener, onceOnly) {
  if (typeof emitter.on === "function") {
    if (onceOnly) emitter.once(name, listener);
    else emitter.on(name, listener);
  } else if (typeof emitter.addEventListener === "function") {
    emitter.addEventListener(name, listener, { once: onceOnly });
  } else {
    throw new TypeError('The "emitter" argument must be an EventEmitter or EventTarget');
  }
}

function remove(emitter, name, listener) {
  if (typeof emitter.removeListener === "function") emitter.removeListener(name, listener);
  else if (typeof emitter.removeEventListener === "function") emitter.removeEventListener(name, listener);
}

function on(emitter, event, options = {}) {
  const signal = options.signal;
  const queue = [];
  const waiting = [];
  let error = null;
  let finished = false;
  const listener = (...args) => {
    const next = waiting.shift();
    if (next) next.resolve({ value: args, done: false });
    else queue.push(args);
  };
  const onError = (err) => {
    finished = true;
    const next = waiting.shift();
    if (next) next.reject(err);
    else error = err;
    iterator.return();
  };
  const onAbort = () => onError(abortError(signal));
  add(emitter, event, listener, false);
  if (event !== "error") add(emitter, "error", onError, false);
  if (signal) signal.addEventListener("abort", onAbort, { once: true });
  const iterator = {
    next() {
      if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
      if (error) {
        const pending = Promise.reject(error);
        error = null;
        return pending;
      }
      if (finished) return Promise.resolve({ value: undefined, done: true });
      return new Promise((resolve, reject) => waiting.push({ resolve, reject }));
    },
    return() {
      remove(emitter, event, listener);
      remove(emitter, "error", onError);
      if (signal) signal.removeEventListener("abort", onAbort);
      finished = true;
      for (const next of waiting.splice(0)) next.resolve({ value: undefined, done: true });
      return Promise.resolve({ value: undefined, done: true });
    },
    throw(err) {
      error = err;
      remove(emitter, event, listener);
      remove(emitter, "error", onError);
      return Promise.reject(err);
    },
    [Symbol.asyncIterator]() {
      return this;
    },
  };
  return iterator;
}

Object.defineProperty(EventEmitter, "defaultMaxListeners", {
  enumerable: true,
  get: () => defaultMaxListeners,
  set: (value) => {
    defaultMaxListeners = value;
  },
});
EventEmitter.EventEmitter = EventEmitter;
EventEmitter.errorMonitor = errorMonitor;
EventEmitter.captureRejectionSymbol = Symbol.for("nodejs.rejection");
EventEmitter.once = once;
EventEmitter.on = on;
EventEmitter.listenerCount = (emitter, type) => emitter.listenerCount(type);
EventEmitter.getEventListeners = (emitter, type) =>
  typeof emitter.listeners === "function" ? emitter.listeners(type) : [];
EventEmitter.setMaxListeners = (n = defaultMaxListeners, ...targets) => {
  if (targets.length === 0) defaultMaxListeners = n;
  for (const target of targets) if (typeof target.setMaxListeners === "function") target.setMaxListeners(n);
};
EventEmitter.addAbortListener = (signal, listener) => {
  signal.addEventListener("abort", listener, { once: true });
  return { [Symbol.dispose || Symbol.for("Symbol.dispose")]: () => signal.removeEventListener("abort", listener) };
};
// No async_hooks here: each resource gets an id, and emit runs in its (only) scope.
let nextAsyncId = 1;
class EventEmitterReferencingAsyncResource {
  #emitter;
  #asyncId = ++nextAsyncId;
  #triggerAsyncId;
  #destroyed = false;
  constructor(emitter, type, triggerAsyncId) {
    this.#emitter = emitter;
    this.type = type;
    this.#triggerAsyncId = triggerAsyncId ?? 1;
  }
  get eventEmitter() {
    return this.#emitter;
  }
  asyncId() {
    return this.#asyncId;
  }
  triggerAsyncId() {
    return this.#triggerAsyncId;
  }
  runInAsyncScope(fn, thisArg, ...args) {
    return Reflect.apply(fn, thisArg, args);
  }
  bind(fn, thisArg) {
    return (...args) => this.runInAsyncScope(fn, thisArg, ...args);
  }
  emitDestroy() {
    this.#destroyed = true;
    return this;
  }
}
EventEmitter.EventEmitterAsyncResource = class EventEmitterAsyncResource extends EventEmitter {
  #resource;
  constructor(options = undefined) {
    let name;
    if (typeof options === "string") {
      name = options;
      options = undefined;
    } else {
      if (options !== undefined && (options === null || typeof options !== "object")) {
        throw Object.assign(new TypeError(`The "options" argument must be of type object. Received ${options === null ? "null" : `type ${typeof options} (${String(options)})`}`), { code: "ERR_INVALID_ARG_TYPE" });
      }
      name = options?.name || new.target.name;
    }
    super(options);
    this.#resource = new EventEmitterReferencingAsyncResource(this, name, options?.triggerAsyncId);
  }
  emit(event, ...args) {
    return this.#resource.runInAsyncScope(super.emit, this, event, ...args);
  }
  emitDestroy() {
    this.#resource.emitDestroy();
  }
  get asyncId() {
    return this.#resource.asyncId();
  }
  get triggerAsyncId() {
    return this.#resource.triggerAsyncId();
  }
  get asyncResource() {
    return this.#resource;
  }
};
module.exports = EventEmitter;
