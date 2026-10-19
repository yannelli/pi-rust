// node:test mocking: MockTracker with fn, method, getter, setter, property, and MockTimers
// (setTimeout, setInterval, setImmediate, and Date on a fake clock).
const { codeError, invalidArgType, invalidArgValue, validate } = require("internal/errors");

const defaultFunction = function () {};

function validateTimes(value, name) {
  if (value === Infinity) return;
  validate.integer(value, name, 1);
}

class MockFunctionContext {
  #calls = [];
  #once = new Map();
  #implementation;
  #restore;
  #times;

  constructor(implementation, restore, times) {
    this.#implementation = implementation;
    this.#restore = restore;
    this.#times = times;
  }

  get calls() {
    return this.#calls.slice();
  }

  callCount() {
    return this.#calls.length;
  }

  mockImplementation(implementation) {
    validate.function(implementation, "implementation");
    this.#implementation = implementation;
  }

  mockImplementationOnce(implementation, onCall) {
    validate.function(implementation, "implementation");
    const next = this.#calls.length;
    const call = onCall ?? next;
    validate.integer(call, "onCall", next);
    this.#once.set(call, implementation);
  }

  restore() {
    const { descriptor, object, original, methodName } = this.#restore;
    if (methodName !== undefined) Object.defineProperty(object, methodName, descriptor);
    else this.#implementation = original;
  }

  resetCalls() {
    this.#calls = [];
  }

  _next() {
    const next = this.#calls.length;
    const implementation = this.#once.get(next) ?? this.#implementation;
    if (next + 1 === this.#times) this.restore();
    this.#once.delete(next);
    return implementation;
  }

  _track(call) {
    this.#calls.push(call);
  }
}

class MockPropertyContext {
  #object;
  #name;
  #descriptor;
  #value;
  #accesses = [];
  #once = new Map();

  constructor(object, name, value) {
    this.#object = object;
    this.#name = name;
    this.#descriptor = Object.getOwnPropertyDescriptor(object, name);
    this.#value = value;
    const self = this;
    Object.defineProperty(object, name, {
      configurable: true,
      enumerable: this.#descriptor ? this.#descriptor.enumerable : true,
      get() {
        const value = self.#once.has(self.#accesses.length) ? self.#once.get(self.#accesses.length) : self.#value;
        self.#once.delete(self.#accesses.length);
        self.#accesses.push({ type: "get", value, stack: new Error() });
        return value;
      },
      set(value) {
        self.#accesses.push({ type: "set", value, stack: new Error() });
        self.#value = value;
      },
    });
  }

  get accesses() {
    return this.#accesses.slice();
  }

  accessCount() {
    return this.#accesses.length;
  }

  mockImplementation(value) {
    this.#value = value;
  }

  mockImplementationOnce(value, onAccess) {
    const next = this.#accesses.length;
    const access = onAccess ?? next;
    validate.integer(access, "onAccess", next);
    this.#once.set(access, value);
  }

  resetAccesses() {
    this.#accesses = [];
  }

  restore() {
    if (this.#descriptor) Object.defineProperty(this.#object, this.#name, this.#descriptor);
    else delete this.#object[this.#name];
  }
}

function findDescriptor(object, name) {
  for (let current = object; current !== null && current !== undefined; current = Object.getPrototypeOf(current)) {
    const descriptor = Object.getOwnPropertyDescriptor(current, name);
    if (descriptor) return descriptor;
  }
  return undefined;
}

class MockTracker {
  #mocks = [];
  #timers;

  get timers() {
    this.#timers ??= new MockTimers();
    return this.#timers;
  }

  #setup(ctx, target) {
    const mock = new Proxy(target, {
      apply(_target, self, args) {
        const fn = ctx._next();
        let result;
        let error;
        try {
          result = Reflect.apply(fn, self, args);
        } catch (thrown) {
          error = thrown;
          throw thrown;
        } finally {
          ctx._track({ arguments: args, error, result, stack: new Error(), target: undefined, this: self });
        }
        return result;
      },
      construct(original, args, newTarget) {
        const fn = ctx._next();
        let result;
        let error;
        try {
          result = Reflect.construct(fn, args, newTarget);
        } catch (thrown) {
          error = thrown;
          throw thrown;
        } finally {
          ctx._track({ arguments: args, error, result, stack: new Error(), target: original, this: result });
        }
        return result;
      },
      get(target, property, receiver) {
        if (property === "mock") return ctx;
        return Reflect.get(target, property, receiver);
      },
    });
    this.#mocks.push(ctx);
    return mock;
  }

  fn(original = function () {}, implementation = original, options = {}) {
    if (original !== null && typeof original === "object") {
      options = original;
      original = function () {};
      implementation = original;
    } else if (implementation !== null && typeof implementation === "object") {
      options = implementation;
      implementation = original;
    }
    validate.function(original, "original");
    validate.function(implementation, "implementation");
    validate.object(options, "options");
    const { times = Infinity } = options;
    validateTimes(times, "options.times");
    return this.#setup(new MockFunctionContext(implementation, { original }, times), original);
  }

  method(object, methodName, implementation = defaultFunction, options = {}) {
    if (typeof methodName !== "string" && typeof methodName !== "symbol") {
      throw invalidArgType("methodName", "one of type string or symbol", methodName);
    }
    if (typeof object !== "function") validate.object(object, "object");
    if (implementation !== null && typeof implementation === "object") {
      options = implementation;
      implementation = defaultFunction;
    }
    validate.function(implementation, "implementation");
    validate.object(options, "options");
    const { getter = false, setter = false, times = Infinity } = options;
    validate.boolean(getter, "options.getter");
    validate.boolean(setter, "options.setter");
    validateTimes(times, "options.times");
    if (setter && getter) throw invalidArgValue("options.setter", setter, "cannot be used with 'options.getter'");
    const descriptor = findDescriptor(object, methodName);
    const original = getter ? descriptor?.get : setter ? descriptor?.set : descriptor?.value;
    if (typeof original !== "function") throw invalidArgValue("methodName", original, "must be a method");
    const restore = { descriptor, object, methodName };
    const ctx = new MockFunctionContext(implementation === defaultFunction ? original : implementation, restore, times);
    const mock = this.#setup(ctx, original);
    const replacement = { configurable: descriptor.configurable, enumerable: descriptor.enumerable };
    if (getter) Object.assign(replacement, { get: mock, set: descriptor.set });
    else if (setter) Object.assign(replacement, { get: descriptor.get, set: mock });
    else Object.assign(replacement, { writable: descriptor.writable, value: mock });
    Object.defineProperty(object, methodName, replacement);
    return mock;
  }

  getter(object, methodName, implementation = defaultFunction, options = {}) {
    if (implementation !== null && typeof implementation === "object") [options, implementation] = [implementation, defaultFunction];
    else validate.object(options, "options");
    if (options.getter === false) throw invalidArgValue("options.getter", false, "cannot be false");
    return this.method(object, methodName, implementation, { ...options, getter: true });
  }

  setter(object, methodName, implementation = defaultFunction, options = {}) {
    if (implementation !== null && typeof implementation === "object") [options, implementation] = [implementation, defaultFunction];
    else validate.object(options, "options");
    if (options.setter === false) throw invalidArgValue("options.setter", false, "cannot be false");
    return this.method(object, methodName, implementation, { ...options, setter: true });
  }

  property(object, propertyName, value) {
    if (object === null || (typeof object !== "object" && typeof object !== "function")) throw invalidArgType("object", "of type object", object);
    const ctx = new MockPropertyContext(object, propertyName, arguments.length > 2 ? value : object[propertyName]);
    this.#mocks.push(ctx);
    const proxy = new Proxy(object, { get: (target, key, receiver) => (key === "mock" ? ctx : Reflect.get(target, key, receiver)) });
    return proxy;
  }

  reset() {
    this.restoreAll();
    this.#timers?.reset();
    this.#mocks = [];
  }

  restoreAll() {
    for (const ctx of this.#mocks) ctx.restore();
  }
}

const TIMER_APIS = ["setTimeout", "setInterval", "setImmediate", "Date"];

// A fake clock: timers run only through tick() and runAll(), in time then creation order.
let timersWarned = false;

class MockTimers {
  #enabled = false;
  #now = 0;
  #queue = [];
  #nextId = 1;
  #saved = {};

  constructor() {
    if (!timersWarned) process.emitWarning("The MockTimers API is an experimental feature and might change at any time", "ExperimentalWarning");
    timersWarned = true;
  }

  #schedule(callback, delay, args, interval) {
    const step = Math.max(1, Number(delay) || 1);
    const timer = { id: this.#nextId++, callback, args, runAt: this.#now + step, step, interval };
    timer.ref = () => timer;
    timer.unref = () => timer;
    timer.hasRef = () => true;
    timer.refresh = () => timer;
    timer[Symbol.toPrimitive] = () => timer.id;
    this.#queue.push(timer);
    return timer;
  }

  #clear(handle) {
    const id = typeof handle === "object" && handle !== null ? handle.id : handle;
    this.#queue = this.#queue.filter((timer) => timer.id !== id);
  }

  #checkEnabled() {
    if (!this.#enabled) throw codeError(Error, "ERR_INVALID_STATE", "You should enable MockTimers first by calling the .enable function");
  }

  enable(options = {}) {
    if (this.#enabled) throw codeError(Error, "ERR_INVALID_STATE", "MockTimers is already enabled!");
    validate.object(options, "options");
    const { apis = TIMER_APIS, now = 0 } = options;
    validate.array(apis, "options.apis");
    for (const api of apis) {
      if (!TIMER_APIS.includes(api)) throw invalidArgValue("options.apis", api, `option ${api} is not supported`);
    }
    this.#enabled = true;
    this.#now = now instanceof Date ? now.getTime() : now;
    const set = (name, value) => {
      this.#saved[name] = globalThis[name];
      globalThis[name] = value;
    };
    if (apis.includes("setTimeout")) {
      set("setTimeout", (callback, delay, ...args) => this.#schedule(callback, delay, args, false));
      set("clearTimeout", (handle) => this.#clear(handle));
    }
    if (apis.includes("setInterval")) {
      set("setInterval", (callback, delay, ...args) => this.#schedule(callback, delay, args, true));
      set("clearInterval", (handle) => this.#clear(handle));
    }
    if (apis.includes("setImmediate")) {
      set("setImmediate", (callback, ...args) => Object.assign(this.#schedule(callback, 0, args, false), { runAt: this.#now }));
      set("clearImmediate", (handle) => this.#clear(handle));
    }
    if (apis.includes("Date")) set("Date", this.#mockDate(this.#saved.Date ?? globalThis.Date));
  }

  #mockDate(RealDate) {
    const timers = this;
    function MockDate(...args) {
      if (!new.target) return new RealDate(timers.#now).toString();
      return args.length === 0 ? new RealDate(timers.#now) : new RealDate(...args);
    }
    Object.setPrototypeOf(MockDate, RealDate);
    MockDate.prototype = RealDate.prototype;
    MockDate.now = () => timers.#now;
    return MockDate;
  }

  tick(milliseconds = 1) {
    this.#checkEnabled();
    validate.integer(milliseconds, "milliseconds", 0);
    const target = this.#now + milliseconds;
    for (;;) {
      const due = this.#queue.filter((timer) => timer.runAt <= target);
      if (due.length === 0) break;
      due.sort((a, b) => a.runAt - b.runAt || a.id - b.id);
      const timer = due[0];
      this.#now = Math.max(this.#now, timer.runAt);
      if (timer.interval) timer.runAt += timer.step;
      else this.#clear(timer);
      timer.callback(...timer.args);
    }
    this.#now = target;
  }

  runAll() {
    this.#checkEnabled();
    const last = this.#queue.reduce((max, timer) => Math.max(max, timer.runAt), this.#now);
    this.tick(last - this.#now);
  }

  setTime(milliseconds = 0) {
    this.#checkEnabled();
    this.#now = milliseconds;
  }

  reset() {
    if (!this.#enabled) return;
    for (const [name, value] of Object.entries(this.#saved)) globalThis[name] = value;
    this.#saved = {};
    this.#queue = [];
    this.#enabled = false;
  }

  [Symbol.dispose]() {
    this.reset();
  }
}

module.exports = { MockTracker, MockTimers, MockFunctionContext, MockPropertyContext };
