// node:perf_hooks: the performance timeline (marks and measures) on the runtime clock.
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
  eventLoopUtilization() {
    throw internal.unsupported("performance.eventLoopUtilization");
  },
  timerify() {
    throw internal.unsupported("performance.timerify");
  },
  toJSON: () => ({ timeOrigin }),
};

function remove(type, name) {
  for (let i = entries.length - 1; i >= 0; i--) {
    if (entries[i].entryType === type && (name === undefined || entries[i].name === name)) entries.splice(i, 1);
  }
}

class PerformanceObserver {
  constructor() {
    throw internal.unsupported("PerformanceObserver");
  }
}

module.exports = {
  performance,
  PerformanceEntry,
  PerformanceObserver,
  monitorEventLoopDelay() {
    throw internal.unsupported("perf_hooks.monitorEventLoopDelay");
  },
  constants: {},
};
