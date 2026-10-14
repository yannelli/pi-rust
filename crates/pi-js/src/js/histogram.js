// perf_hooks histograms. Values are integers; percentiles keep three significant digits, as
// Node's HDR histogram does.
const errors = require("internal/errors");
const kEmptyMin = 9223372036854776000;

function bucket(value) {
  if (value < 1000) return value;
  const scale = 10 ** (Math.floor(Math.log10(value)) - 2);
  return Math.round(value / scale) * scale;
}

class Histogram {
  #counts = new Map();
  #count = 0;
  #min = kEmptyMin;
  #max = 0;
  #sum = 0;
  #squares = 0;
  #exceeds = 0;
  #last;
  get count() {
    return this.#count;
  }
  get countBigInt() {
    return BigInt(this.#count);
  }
  get min() {
    return this.#min;
  }
  get minBigInt() {
    return BigInt(this.#min);
  }
  get max() {
    return this.#max;
  }
  get maxBigInt() {
    return BigInt(this.#max);
  }
  get mean() {
    return this.#count === 0 ? NaN : this.#sum / this.#count;
  }
  get stddev() {
    if (this.#count === 0) return NaN;
    const mean = this.#sum / this.#count;
    return Math.sqrt(Math.max(0, this.#squares / this.#count - mean * mean));
  }
  get exceeds() {
    return this.#exceeds;
  }
  get exceedsBigInt() {
    return BigInt(this.#exceeds);
  }
  percentile(percentile) {
    if (typeof percentile !== "number") throw errors.invalidArgType("percentile", "of type number", percentile);
    if (!(percentile > 0 && percentile <= 100)) throw errors.outOfRange("percentile", "> 0 && <= 100", percentile);
    if (this.#count === 0) return 0;
    const target = Math.max(1, Math.ceil((percentile / 100) * this.#count));
    if (target >= this.#count) return this.#max;
    let seen = 0;
    for (const value of [...this.#counts.keys()].sort((a, b) => a - b)) {
      seen += this.#counts.get(value);
      if (seen >= target) return Math.min(Math.max(value, this.#min), this.#max);
    }
    return this.#max;
  }
  percentileBigInt(percentile) {
    return BigInt(this.percentile(percentile));
  }
  get percentiles() {
    const result = new Map();
    if (this.#count === 0) return result.set(100, 0);
    result.set(0, this.#min);
    for (let step = 50; ; step /= 2) {
      const percentile = 100 - step;
      const value = this.percentile(percentile);
      result.set(percentile, value);
      if (value >= this.#max || step < 1e-6) break;
    }
    return result.set(100, this.#max);
  }
  get percentilesBigInt() {
    return new Map([...this.percentiles].map(([key, value]) => [key, BigInt(value)]));
  }
  reset() {
    this.#counts.clear();
    this.#count = this.#max = this.#sum = this.#squares = this.#exceeds = 0;
    this.#min = kEmptyMin;
  }
  toJSON() {
    return { count: this.count, min: this.min, max: this.max, mean: this.mean, exceeds: this.exceeds, stddev: this.stddev, percentiles: Object.fromEntries(this.percentiles) };
  }
  _record(value) {
    const key = bucket(value);
    this.#counts.set(key, (this.#counts.get(key) || 0) + 1);
    this.#count++;
    this.#sum += value;
    this.#squares += value * value;
    if (value < this.#min) this.#min = value;
    if (value > this.#max) this.#max = value;
  }
  _exceeded() {
    this.#exceeds++;
  }
  _entries() {
    return this.#counts;
  }
  _mark() {
    const now = process.hrtime.bigint();
    const delta = this.#last === undefined ? undefined : Number(now - this.#last);
    this.#last = now;
    return delta;
  }
}

class RecordableHistogram extends Histogram {
  record(value) {
    if (typeof value === "bigint") value = Number(value);
    else if (typeof value !== "number") throw errors.invalidArgType("val", "of type number or bigint", value);
    if (!Number.isInteger(value) || value < 1 || value > Number.MAX_SAFE_INTEGER) {
      throw errors.outOfRange("val", `>= 1 && <= ${Number.MAX_SAFE_INTEGER}`, value);
    }
    this._record(value);
  }
  recordDelta() {
    const delta = this._mark();
    if (delta !== undefined) this._record(Math.max(1, delta));
  }
  add(other) {
    if (!(other instanceof RecordableHistogram)) throw errors.invalidArgType("other", "an instance of RecordableHistogram", other);
    for (const [value, count] of other._entries()) for (let i = 0; i < count; i++) this._record(value);
  }
}

// monitorEventLoopDelay: an unreferenced timer measures how late each tick fires.
class IntervalHistogram extends Histogram {
  #resolution;
  #timer = null;
  #expected = 0;
  constructor(resolution) {
    super();
    this.#resolution = resolution;
  }
  enable() {
    if (this.#timer) return false;
    this.#expected = performance.now() + this.#resolution;
    this.#timer = setInterval(() => {
      const now = performance.now();
      this._record(Math.max(1, Math.round((now - this.#expected + this.#resolution) * 1e6)));
      this.#expected = now + this.#resolution;
    }, this.#resolution);
    this.#timer.unref();
    return true;
  }
  disable() {
    if (!this.#timer) return false;
    clearInterval(this.#timer);
    this.#timer = null;
    return true;
  }
  [Symbol.dispose]() {
    this.disable();
  }
}

function createHistogram(options = {}) {
  errors.validate.object(options, "options");
  const { lowest = 1, highest = Number.MAX_SAFE_INTEGER, figures = 3 } = options;
  for (const [name, value] of [["options.lowest", lowest], ["options.highest", highest]]) {
    if (typeof value !== "number" && typeof value !== "bigint") throw errors.invalidArgType(name, "of type number or bigint", value);
  }
  errors.validate.integer(figures, "options.figures", 1, 5);
  return new RecordableHistogram();
}

function monitorEventLoopDelay(options = {}) {
  errors.validate.object(options, "options");
  const { resolution = 10 } = options;
  errors.validate.integer(resolution, "options.resolution", 1);
  return new IntervalHistogram(resolution);
}

module.exports = { Histogram, RecordableHistogram, IntervalHistogram, createHistogram, monitorEventLoopDelay };
