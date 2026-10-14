// node:diagnostics_channel: named publish and subscribe channels, bound stores, and tracing.
const { invalidArgType, validate } = require("internal/errors");
const channels = new Map();

// A store is anything with run(value, fn), such as AsyncLocalStorage.
function wrapStoreRun(store, data, next, transform) {
  return () => {
    let context;
    try {
      context = transform(data);
    } catch (error) {
      process.nextTick(() => {
        throw error;
      });
      return next();
    }
    return store.run(context, next);
  };
}

class Channel {
  #subscribers = [];
  #stores = new Map();
  constructor(name) {
    this.name = name;
  }
  get hasSubscribers() {
    return this.#subscribers.length > 0 || this.#stores.size > 0;
  }
  subscribe(subscription) {
    validate.function(subscription, "subscription");
    this.#subscribers.push(subscription);
  }
  unsubscribe(subscription) {
    const index = this.#subscribers.indexOf(subscription);
    if (index < 0) return false;
    this.#subscribers.splice(index, 1);
    return true;
  }
  publish(message) {
    for (const subscriber of this.#subscribers.slice()) {
      try {
        subscriber(message, this.name);
      } catch (error) {
        process.nextTick(() => {
          throw error;
        });
      }
    }
  }
  bindStore(store, transform) {
    if (transform !== undefined) validate.function(transform, "transform");
    this.#stores.set(store, transform || ((data) => data));
  }
  unbindStore(store) {
    return this.#stores.delete(store);
  }
  runStores(data, fn, thisArg, ...args) {
    let run = () => {
      this.publish(data);
      return Reflect.apply(fn, thisArg, args);
    };
    for (const [store, transform] of this.#stores) run = wrapStoreRun(store, data, run, transform);
    return run();
  }
}

function channel(name) {
  if (typeof name !== "string" && typeof name !== "symbol") throw invalidArgType("channel", "of type string or symbol", name);
  let existing = channels.get(name);
  if (!existing) {
    existing = new Channel(name);
    channels.set(name, existing);
  }
  return existing;
}

const TRACE_EVENTS = ["start", "end", "asyncStart", "asyncEnd", "error"];

class TracingChannel {
  constructor(nameOrChannels) {
    if (typeof nameOrChannels === "string") {
      for (const event of TRACE_EVENTS) this[event] = channel(`tracing:${nameOrChannels}:${event}`);
    } else if (nameOrChannels !== null && typeof nameOrChannels === "object") {
      for (const event of TRACE_EVENTS) {
        const value = nameOrChannels[event];
        if (!(value instanceof Channel)) throw invalidArgType(`nameOrChannels.${event}`, "an instance of Channel", value);
        this[event] = value;
      }
    } else {
      throw invalidArgType("nameOrChannels", "of type string or an instance of TracingChannel or Object", nameOrChannels);
    }
  }
  get hasSubscribers() {
    return TRACE_EVENTS.some((event) => this[event].hasSubscribers);
  }
  subscribe(handlers) {
    for (const event of TRACE_EVENTS) if (handlers[event]) this[event].subscribe(handlers[event]);
  }
  unsubscribe(handlers) {
    let done = true;
    for (const event of TRACE_EVENTS) if (handlers[event] && !this[event].unsubscribe(handlers[event])) done = false;
    return done;
  }
  traceSync(fn, context = {}, thisArg, ...args) {
    if (!this.hasSubscribers) return Reflect.apply(fn, thisArg, args);
    const { start, end, error } = this;
    return start.runStores(context, () => {
      try {
        const result = Reflect.apply(fn, thisArg, args);
        context.result = result;
        return result;
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }
  tracePromise(fn, context = {}, thisArg, ...args) {
    if (!this.hasSubscribers) return Reflect.apply(fn, thisArg, args);
    const { start, end, asyncStart, asyncEnd, error } = this;
    const reject = (err) => {
      context.error = err;
      error.publish(context);
      asyncStart.publish(context);
      asyncEnd.publish(context);
      return Promise.reject(err);
    };
    const resolve = (result) => {
      context.result = result;
      asyncStart.publish(context);
      asyncEnd.publish(context);
      return result;
    };
    return start.runStores(context, () => {
      try {
        let promise = Reflect.apply(fn, thisArg, args);
        if (!(promise instanceof Promise)) promise = Promise.resolve(promise);
        return promise.then(resolve, reject);
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }
  traceCallback(fn, position = -1, context = {}, thisArg, ...args) {
    if (!this.hasSubscribers) return Reflect.apply(fn, thisArg, args);
    const { start, end, asyncStart, asyncEnd, error } = this;
    const callback = args.at(position);
    validate.function(callback, "callback");
    function wrappedCallback(err, res) {
      if (err) {
        context.error = err;
        error.publish(context);
      } else {
        context.result = res;
      }
      return asyncStart.runStores(context, () => {
        try {
          return Reflect.apply(callback, this, arguments);
        } finally {
          asyncEnd.publish(context);
        }
      });
    }
    args.splice(position, 1, wrappedCallback);
    return start.runStores(context, () => {
      try {
        return Reflect.apply(fn, thisArg, args);
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }
}

module.exports = {
  channel,
  hasSubscribers: (name) => channels.has(name) && channels.get(name).hasSubscribers,
  subscribe: (name, subscription) => channel(name).subscribe(subscription),
  unsubscribe: (name, subscription) => (channels.has(name) ? channels.get(name).unsubscribe(subscription) : false),
  tracingChannel: (nameOrChannels) => new TracingChannel(nameOrChannels),
  Channel,
};
