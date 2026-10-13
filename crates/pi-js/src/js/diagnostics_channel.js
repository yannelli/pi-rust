// node:diagnostics_channel: named publish and subscribe channels.
const channels = new Map();

class Channel {
  #subscribers = [];
  constructor(name) {
    this.name = name;
  }
  get hasSubscribers() {
    return this.#subscribers.length > 0;
  }
  subscribe(subscription) {
    if (typeof subscription !== "function") throw Object.assign(new TypeError('The "subscription" argument must be of type function'), { code: "ERR_INVALID_ARG_TYPE" });
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
  bindStore() {
    throw internal.unsupported("diagnostics_channel bindStore");
  }
  runStores(message, fn, thisArg, ...args) {
    this.publish(message);
    return Reflect.apply(fn, thisArg, args);
  }
}

function channel(name) {
  let existing = channels.get(name);
  if (!existing) {
    existing = new Channel(name);
    channels.set(name, existing);
  }
  return existing;
}

module.exports = {
  channel,
  hasSubscribers: (name) => channels.has(name) && channels.get(name).hasSubscribers,
  subscribe: (name, subscription) => channel(name).subscribe(subscription),
  unsubscribe: (name, subscription) => channel(name).unsubscribe(subscription),
  tracingChannel() {
    throw internal.unsupported("diagnostics_channel.tracingChannel");
  },
  Channel,
};
