// Intl: the subset extensions and pi-tui use. Segmenter follows Unicode segmentation (UAX #29);
// NumberFormat, DateTimeFormat, PluralRules, and Collator implement en-US behavior only.
const encoding = native.encoding;
const WORD_LIKE = /[\p{L}\p{N}]/u;

function segmentsOf(input, granularity) {
  const text = String(input);
  const parts = encoding.segment(text, granularity);
  const segments = new Array(parts.length);
  let index = 0;
  for (let i = 0; i < parts.length; i++) {
    const segment = { segment: parts[i], index, input: text };
    if (granularity === "word") segment.isWordLike = WORD_LIKE.test(parts[i]);
    segments[i] = segment;
    index += parts[i].length;
  }
  return segments;
}

class Segments {
  #list;
  constructor(list) {
    this.#list = list;
  }
  containing(codeUnitIndex = 0) {
    const index = Number(codeUnitIndex);
    return this.#list.find((segment) => index >= segment.index && index < segment.index + segment.segment.length);
  }
  [Symbol.iterator]() {
    return this.#list[Symbol.iterator]();
  }
}

class Segmenter {
  #granularity;
  constructor(_locales, options = {}) {
    const granularity = options.granularity ?? "grapheme";
    if (!["grapheme", "word", "sentence"].includes(granularity)) {
      throw new RangeError(`Value ${granularity} out of range for Intl.Segmenter options property granularity`);
    }
    this.#granularity = granularity;
  }
  segment(input) {
    return new Segments(segmentsOf(input, this.#granularity));
  }
  resolvedOptions() {
    return { locale: "en-US", granularity: this.#granularity };
  }
  static supportedLocalesOf() {
    return ["en-US"];
  }
}

function group(digits) {
  return digits.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

class NumberFormat {
  #options;
  constructor(_locales, options = {}) {
    this.#options = { ...options };
  }
  format(value) {
    const options = this.#options;
    let number = Number(value);
    if (!Number.isFinite(number)) return String(number);
    const style = options.style ?? "decimal";
    if (style === "percent") number *= 100;
    const defaultMax = { currency: 2, percent: 0 }[style] ?? 3;
    const min = options.minimumFractionDigits ?? (style === "currency" ? 2 : 0);
    const max = Math.max(min, options.maximumFractionDigits ?? defaultMax);
    let text = Math.abs(number).toFixed(max);
    if (text.includes(".")) {
      let [whole, fraction] = text.split(".");
      while (fraction.length > min && fraction.endsWith("0")) fraction = fraction.slice(0, -1);
      text = fraction.length > 0 ? `${whole}.${fraction}` : whole;
    }
    const [whole, fraction] = text.split(".");
    text = (options.useGrouping === false ? whole : group(whole)) + (fraction ? `.${fraction}` : "");
    const sign = number < 0 && Number(text.replace(/,/g, "")) !== 0 ? "-" : "";
    if (style === "percent") return `${sign}${text}%`;
    if (style === "currency") {
      const symbol = !options.currency || options.currency === "USD" ? "$" : `${options.currency} `;
      return `${sign}${symbol}${text}`;
    }
    if (style === "unit" && options.unit) return `${sign}${text} ${options.unit}`;
    return sign + text;
  }
  formatToParts(value) {
    return [{ type: "literal", value: this.format(value) }];
  }
  resolvedOptions() {
    return { locale: "en-US", numberingSystem: "latn", style: "decimal", ...this.#options };
  }
  static supportedLocalesOf() {
    return ["en-US"];
  }
}

const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
  "November", "December"];
const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

class DateTimeFormat {
  #options;
  constructor(_locales, options = {}) {
    this.#options = { ...options };
  }
  format(value) {
    const date = value === undefined ? new Date() : new Date(value);
    const options = this.#options;
    const hasDate = options.year || options.month || options.day || options.weekday || options.dateStyle;
    const hasTime = options.hour || options.minute || options.second || options.timeStyle;
    if (!hasDate && !hasTime) return date.toLocaleDateString();
    const name = (names, index, width) => (width === "long" ? names[index] : names[index].slice(0, 3));
    const parts = [];
    if (hasDate) {
      if (options.weekday) parts.push(name(WEEKDAYS, date.getDay(), options.weekday));
      if (options.month === "long" || options.month === "short") {
        const day = options.day ? ` ${date.getDate()}` : "";
        const year = options.year ? `, ${date.getFullYear()}` : "";
        parts.push(`${name(MONTHS, date.getMonth(), options.month)}${day}${year}`);
      } else {
        parts.push(date.toLocaleDateString());
      }
    }
    if (hasTime) parts.push(date.toLocaleTimeString());
    return parts.join(", ");
  }
  formatToParts(value) {
    return [{ type: "literal", value: this.format(value) }];
  }
  resolvedOptions() {
    const timeZone = process.env.TZ || "UTC";
    return { locale: "en-US", calendar: "gregory", numberingSystem: "latn", timeZone, ...this.#options };
  }
  static supportedLocalesOf() {
    return ["en-US"];
  }
}

class PluralRules {
  select(value) {
    return Number(value) === 1 ? "one" : "other";
  }
  resolvedOptions() {
    return { locale: "en-US", type: "cardinal" };
  }
}

class Collator {
  #options;
  constructor(_locales, options = {}) {
    this.#options = options;
  }
  compare = (left, right) => {
    const caseless = this.#options.sensitivity === "base" || this.#options.sensitivity === "accent";
    const normalize = (value) => (caseless ? String(value).toLowerCase() : String(value));
    const split = (value) => (this.#options.numeric ? value.split(/(\d+)/) : [value]);
    const leftParts = split(normalize(left));
    const rightParts = split(normalize(right));
    for (let i = 0; i < Math.max(leftParts.length, rightParts.length); i++) {
      const a = leftParts[i] ?? "";
      const b = rightParts[i] ?? "";
      const order = i % 2 === 1 ? Number(a) - Number(b) : a < b ? -1 : Number(a > b);
      if (order !== 0) return Math.sign(order);
    }
    return 0;
  };
  resolvedOptions() {
    return { locale: "en-US", usage: "sort", sensitivity: "variant", ...this.#options };
  }
}

module.exports = {
  Segmenter,
  NumberFormat,
  DateTimeFormat,
  PluralRules,
  Collator,
  getCanonicalLocales: (locales) => (locales === undefined ? [] : [].concat(locales)),
  supportedValuesOf: () => [],
};
