// node:timers and timers/promises on the Rust event loop's timer queue.
const timers = native.timers;

function checkCallback(callback) {
  if (typeof callback !== "function") {
    const error = new TypeError(`The "callback" argument must be of type function. Received ${typeof callback}`);
    error.code = "ERR_INVALID_ARG_TYPE";
    throw error;
  }
}

const MAX_DELAY = 2147483647;

// Node turns oversized delays into 1ms. Below 1ms the timer runs on the next loop turn.
function delayOf(value) {
  const delay = Number(value);
  if (delay > MAX_DELAY) return 1;
  if (delay >= 1) return delay;
  return 0;
}

class Timeout {
  constructor(callback, delay, args, repeat) {
    this._onTimeout = callback;
    this._idleTimeout = delay;
    this._repeat = repeat ? delay : null;
    this._destroyed = false;
    const run = () => {
      if (!repeat) this._destroyed = true;
      callback.apply(this, args);
    };
    this._id = timers.start(run, delay, repeat);
  }
  ref() {
    timers.ref(this._id, true);
    return this;
  }
  unref() {
    timers.ref(this._id, false);
    return this;
  }
  hasRef() {
    return timers.hasRef(this._id);
  }
  refresh() {
    timers.refresh(this._id);
    return this;
  }
  close() {
    clearTimeout(this);
    return this;
  }
  [Symbol.toPrimitive]() {
    return this._id;
  }
}
if (Symbol.dispose) Timeout.prototype[Symbol.dispose] = function dispose() {
  clearTimeout(this);
};

const byId = new Map();

function setTimeout(callback, delay, ...args) {
  checkCallback(callback);
  const timeout = new Timeout(() => {
    byId.delete(timeout._id);
    callback(...args);
  }, delayOf(delay), [], false);
  byId.set(timeout._id, timeout);
  return timeout;
}

function setInterval(callback, delay, ...args) {
  checkCallback(callback);
  const timeout = new Timeout(callback, delayOf(delay), args, true);
  byId.set(timeout._id, timeout);
  return timeout;
}

function clearTimeout(timeout) {
  if (timeout === null || timeout === undefined) return;
  const id = typeof timeout === "object" ? timeout._id : Number(timeout);
  if (typeof id !== "number" || Number.isNaN(id)) return;
  const known = byId.get(id);
  if (known) known._destroyed = true;
  byId.delete(id);
  timers.clear(id);
}

class Immediate {
  constructor(callback, args) {
    this._onImmediate = callback;
    this._id = timers.immediate(() => callback(...args));
  }
  ref() {
    return this;
  }
  unref() {
    return this;
  }
  hasRef() {
    return true;
  }
}

function setImmediate(callback, ...args) {
  checkCallback(callback);
  return new Immediate(callback, args);
}

function clearImmediate(immediate) {
  if (immediate && typeof immediate === "object") timers.clearImmediate(immediate._id);
}

function abortError(signal) {
  const error = new Error("The operation was aborted");
  error.name = "AbortError";
  error.code = "ABORT_ERR";
  if (signal && signal.reason !== undefined) error.cause = signal.reason;
  return error;
}

// Settle a promise from a scheduling function, honoring AbortSignal and ref options.
function scheduled(schedule, cancel, value, options = {}) {
  const signal = options.signal;
  if (signal && signal.aborted) return Promise.reject(abortError(signal));
  return new Promise((resolve, reject) => {
    let onAbort;
    const handle = schedule(() => {
      if (signal) signal.removeEventListener("abort", onAbort);
      resolve(value);
    });
    if (options.ref === false && handle.unref) handle.unref();
    if (signal) {
      onAbort = () => {
        cancel(handle);
        reject(abortError(signal));
      };
      signal.addEventListener("abort", onAbort, { once: true });
    }
  });
}

const promises = {
  setTimeout: (delay, value, options) => scheduled((done) => setTimeout(done, delay), clearTimeout, value, options),
  setImmediate: (value, options) => scheduled((done) => setImmediate(done), clearImmediate, value, options),
  setInterval(delay, value, options = {}) {
    const signal = options.signal;
    let pending = 0;
    let waiter = null;
    let failure = null;
    const interval = setInterval(() => {
      if (waiter) {
        const resolve = waiter;
        waiter = null;
        resolve({ value, done: false });
      } else {
        pending++;
      }
    }, delay);
    if (options.ref === false) interval.unref();
    const finish = () => {
      clearInterval(interval);
      return Promise.resolve({ value: undefined, done: true });
    };
    if (signal) signal.addEventListener("abort", () => {
      failure = abortError(signal);
      clearInterval(interval);
      if (waiter) waiter(Promise.reject(failure));
    }, { once: true });
    return {
      next() {
        if (failure) return Promise.reject(failure);
        if (pending > 0) {
          pending--;
          return Promise.resolve({ value, done: false });
        }
        return new Promise((resolve) => {
          waiter = resolve;
        });
      },
      return: finish,
      [Symbol.asyncIterator]() {
        return this;
      },
    };
  },
  scheduler: {
    wait: (delay, options) => scheduled((done) => setTimeout(done, delay), clearTimeout, undefined, options),
    yield: () => scheduled((done) => setImmediate(done), clearImmediate, undefined),
  },
};

module.exports = {
  setTimeout,
  clearTimeout,
  setInterval,
  clearInterval: clearTimeout,
  setImmediate,
  clearImmediate,
  Timeout,
  Immediate,
  promises,
};
