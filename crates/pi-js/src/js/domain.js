// node:domain (deprecated): the active domain follows async context, as Node's does with
// async_hooks; errors thrown under a domain go to its 'error' event.
const EventEmitter = require("events");
const { AsyncLocalStorage } = require("async_hooks");

const storage = new AsyncLocalStorage();
const stack = [];

function activeDomain() {
  return storage.getStore();
}

Object.defineProperty(process, "domain", {
  configurable: true,
  enumerable: true,
  get: activeDomain,
  set() {},
});

function routeError(domain, error) {
  if (error !== null && typeof error === "object") {
    error.domain = domain;
    error.domainThrown = true;
  }
  domain.emit("error", error);
  return true;
}

internal.domainHandler = function domainHandler(error) {
  let domain = activeDomain();
  const frame = internal.thrownFrame(error);
  if (frame !== undefined && frame.has(storage)) domain = frame.get(storage);
  if (!domain || domain.listenerCount("error") === 0) return false;
  return routeError(domain, error);
};

class Domain extends EventEmitter {
  constructor() {
    super();
    this.members = [];
  }
  enter() {
    stack.push(this);
    exports.active = this;
    storage.enterWith(this);
  }
  exit() {
    const index = stack.lastIndexOf(this);
    if (index === -1) return;
    stack.splice(index);
    const top = stack.length > 0 ? stack[stack.length - 1] : undefined;
    exports.active = top;
    storage.enterWith(top);
  }
  add(emitter) {
    if (emitter.domain === this) return;
    if (emitter.domain) emitter.domain.remove(emitter);
    emitter.domain = this;
    this.members.push(emitter);
  }
  remove(emitter) {
    emitter.domain = null;
    const index = this.members.indexOf(emitter);
    if (index !== -1) this.members.splice(index, 1);
  }
  run(fn, ...args) {
    this.enter();
    try {
      return storage.run(this, () => Reflect.apply(fn, this, args));
    } finally {
      this.exit();
    }
  }
  bind(callback) {
    const domain = this;
    function runBound(...args) {
      return domain.run(() => Reflect.apply(callback, this, args));
    }
    runBound.domain = this;
    return runBound;
  }
  intercept(callback) {
    const domain = this;
    function runIntercepted(error, ...args) {
      if (error) {
        if (typeof error === "object") {
          error.domainBound = callback;
          error.domainThrown = false;
          error.domain = domain;
        }
        domain.emit("error", error);
        return undefined;
      }
      return domain.run(() => Reflect.apply(callback, this, args));
    }
    runIntercepted.domain = this;
    return runIntercepted;
  }
  dispose() {
    this.exit();
    this.removeAllListeners();
    this.members.length = 0;
  }
}
Domain.prototype.domain = null;

// Emitters made while a domain is active join it; their unhandled 'error' events go to it.
const init = EventEmitter.init;
EventEmitter.init = function initWithDomain(options) {
  init.call(this, options);
  const domain = activeDomain();
  if (domain && !(this instanceof Domain) && this.domain === undefined) this.domain = domain;
};
const emit = EventEmitter.prototype.emit;
EventEmitter.prototype.emit = function emitWithDomain(type, ...args) {
  if (type === "error" && this.domain && this.listenerCount("error") === 0 && !(this instanceof Domain)) {
    const error = args[0];
    if (error !== null && typeof error === "object") {
      error.domainEmitter = this;
      error.domainThrown = false;
      error.domain = this.domain;
    }
    this.domain.emit("error", error);
    return false;
  }
  return Reflect.apply(emit, this, [type, ...args]);
};
EventEmitter.usingDomains = true;

exports.active = null;
exports.Domain = Domain;
exports.create = exports.createDomain = () => new Domain();
exports._stack = stack;
