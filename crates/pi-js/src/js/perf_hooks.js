// node:perf_hooks: the performance timeline (marks, measures, functions) on the runtime clock.
const errors = require("internal/errors");
const histograms = require("internal/histogram");
function now() {
  const [seconds, nanos] = native.process.hrtime();
  return seconds * 1000 + nanos / 1e6;
}
const timeOrigin = Date.now() - now();
const entries = [];

class PerformanceEntry {
  constructor(name, entryType, startTime, duration, detail) {
    this.name = name;
    this.entryType = entryType;
    this.startTime = startTime;
    this.duration = duration;
    this.detail = detail === undefined ? null : detail;
  }
  toJSON() {
    return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration, detail: this.detail };
  }
}

function markTime(name) {
  if (name === undefined) return undefined;
  if (typeof name === "number") return name;
  const mark = entries.filter((entry) => entry.entryType === "mark" && entry.name === name).pop();
  if (!mark) throw new SyntaxError(`The "${name}" performance mark has not been set`);
  return mark.startTime;
}

const performance = {
  now,
  timeOrigin,
  mark(name, options = {}) {
    const entry = new PerformanceEntry(String(name), "mark", options.startTime ?? now(), 0, options.detail);
    entries.push(entry);
    enqueue(entry);
    return entry;
  },
  measure(name, start, end) {
    let detail;
    if (start && typeof start === "object") {
      detail = start.detail;
      end = start.end;
      start = start.start;
    }
    const startTime = markTime(start) ?? 0;
    const endTime = markTime(end) ?? now();
    const entry = new PerformanceEntry(String(name), "measure", startTime, endTime - startTime, detail);
    entries.push(entry);
    enqueue(entry);
    return entry;
  },
  getEntries: () => entries.slice(),
  getEntriesByName: (name, type) => entries.filter((entry) => entry.name === name && (!type || entry.entryType === type)),
  getEntriesByType: (type) => entries.filter((entry) => entry.entryType === type),
  clearMarks(name) {
    remove("mark", name);
  },
  clearMeasures(name) {
    remove("measure", name);
  },
  eventLoopUtilization,
  timerify,
  toJSON: () => ({ timeOrigin }),
};

function remove(type, name) {
  for (let i = entries.length - 1; i >= 0; i--) {
    if (entries[i].entryType === type && (name === undefined || entries[i].name === name)) entries.splice(i, 1);
  }
}

function eventLoopUtilization(util1, util2) {
  const [loopStart, idleTime] = native.process.loopTimes();
  if (loopStart <= 0) return { idle: 0, active: 0, utilization: 0 };
  if (util2) {
    const idle = util1.idle - util2.idle;
    const active = util1.active - util2.active;
    return { idle, active, utilization: active / (idle + active) };
  }
  const idle = idleTime;
  const active = now() - loopStart - idle;
  if (!util1) return { idle, active, utilization: active / (idle + active) };
  const idleDelta = idle - util1.idle;
  const activeDelta = active - util1.active;
  return { idle: idleDelta, active: activeDelta, utilization: activeDelta / (idleDelta + activeDelta) };
}

function timerify(fn, options = {}) {
  errors.validate.function(fn, "fn");
  errors.validate.object(options, "options");
  const { histogram } = options;
  if (histogram !== undefined && !(histogram instanceof histograms.RecordableHistogram)) {
    throw errors.invalidArgType("options.histogram", "an instance of RecordableHistogram", histogram);
  }
  const complete = (start, args) => {
    const duration = now() - start;
    if (histogram) histogram.record(Math.max(1, Math.ceil(duration * 1e6)));
    enqueue(new PerformanceEntry(fn.name, "function", start, duration, args));
  };
  function timerified(...args) {
    const start = now();
    const result = new.target ? Reflect.construct(fn, args, fn) : Reflect.apply(fn, this, args);
    if (!new.target && result instanceof Promise) return result.finally(() => complete(start, args));
    complete(start, args);
    return result;
  }
  Object.defineProperties(timerified, {
    length: { configurable: false, enumerable: true, value: fn.length },
    name: { configurable: false, enumerable: true, value: `timerified ${fn.name}` },
  });
  return timerified;
}

// Observers get entries in a batch on a later task, as in Node.
const ENTRY_TYPES = ["function", "mark", "measure"];
const observers = new Set();
let flushScheduled = false;
function enqueue(entry) {
  let any = false;
  for (const observer of observers) any = observer._offer(entry) || any;
  if (any && !flushScheduled) {
    flushScheduled = true;
    setImmediate(() => {
      flushScheduled = false;
      for (const observer of [...observers]) observer._flush();
    });
  }
}

class PerformanceObserverEntryList {
  #entries;
  constructor(list) {
    this.#entries = list;
  }
  getEntries() {
    return this.#entries.slice();
  }
  getEntriesByType(type) {
    return this.#entries.filter((entry) => entry.entryType === String(type));
  }
  getEntriesByName(name, type) {
    return this.#entries.filter((entry) => entry.name === String(name) && (type === undefined || entry.entryType === String(type)));
  }
}

class PerformanceObserver {
  #callback;
  #types = new Set();
  #buffer = [];
  constructor(callback) {
    errors.validate.function(callback, "callback");
    this.#callback = callback;
  }
  static get supportedEntryTypes() {
    return ENTRY_TYPES.slice();
  }
  observe(options = {}) {
    errors.validate.object(options, "options");
    const { entryTypes, type, buffered } = options;
    if (entryTypes === undefined && type === undefined) throw errors.missingArgs("options.entryTypes", "options.type");
    if (entryTypes !== undefined && type !== undefined) {
      throw errors.invalidArgValue("options.entryTypes", entryTypes, "options.entryTypes can not set with options.type together");
    }
    this.#types = new Set((entryTypes === undefined ? [type] : entryTypes).filter((name) => ENTRY_TYPES.includes(name)));
    if (this.#types.size === 0) return;
    observers.add(this);
    if (buffered) for (const entry of entries) enqueue(entry);
  }
  disconnect() {
    observers.delete(this);
    this.#buffer = [];
  }
  takeRecords() {
    const list = this.#buffer;
    this.#buffer = [];
    return list;
  }
  _offer(entry) {
    if (!this.#types.has(entry.entryType)) return false;
    this.#buffer.push(entry);
    return true;
  }
  _flush() {
    if (this.#buffer.length === 0) return;
    const list = new PerformanceObserverEntryList(this.takeRecords());
    this.#callback.call(this, list, this);
  }
}

module.exports = {
  performance,
  PerformanceEntry,
  PerformanceObserver,
  PerformanceObserverEntryList,
  monitorEventLoopDelay: histograms.monitorEventLoopDelay,
  createHistogram: histograms.createHistogram,
  constants: {},
};
