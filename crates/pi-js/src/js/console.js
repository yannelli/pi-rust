// node:console: the global console and the Console class.
const { formatWithOptions, inspect } = require("internal/inspect");

function nativeStream(name) {
  return {
    write(text) {
      native.io.writeText(name, text);
      return true;
    },
    get isTTY() {
      return native.io.isTTY(name);
    },
  };
}

function cellText(value) {
  return typeof value === "string" ? inspect(value) : inspect(value, { depth: 1, breakLength: Infinity });
}

function renderTable(head, rows) {
  const widths = head.map((title, column) => Math.max(title.length, ...rows.map((row) => (row[column] || "").length)) + 2);
  const line = (left, middle, right) => left + widths.map((width) => "─".repeat(width)).join(middle) + right;
  const render = (cells) => "│" + cells.map((cell, column) => ` ${(cell || "").padEnd(widths[column] - 1)}`).join("│") + "│";
  return [line("┌", "┬", "┐"), render(head), line("├", "┼", "┤"), ...rows.map(render), line("└", "┴", "┘")].join("\n");
}

class Console {
  constructor(stdout, stderr, ignoreErrors) {
    let options = stdout;
    if (!stdout || typeof stdout.write === "function") options = { stdout, stderr, ignoreErrors };
    this._stdout = options.stdout || nativeStream("stdout");
    this._stderr = options.stderr || options.stdout || nativeStream("stderr");
    this._colorMode = options.colorMode === undefined ? "auto" : options.colorMode;
    this._inspectOptions = options.inspectOptions || {};
    this._groupIndent = "";
    this._counts = new Map();
    this._timers = new Map();
    for (const name of Object.getOwnPropertyNames(Console.prototype)) {
      if (name !== "constructor" && typeof this[name] === "function") this[name] = this[name].bind(this);
    }
  }
  _write(stream, args) {
    const target = stream === "stderr" ? this._stderr : this._stdout;
    const colors = this._colorMode === "auto" ? Boolean(target.isTTY) : Boolean(this._colorMode);
    let text = formatWithOptions(Object.assign({ colors }, this._inspectOptions), ...args);
    if (this._groupIndent) text = this._groupIndent + text.split("\n").join("\n" + this._groupIndent);
    target.write(text + "\n");
  }
  log(...args) {
    this._write("stdout", args);
  }
  info(...args) {
    this._write("stdout", args);
  }
  debug(...args) {
    this._write("stdout", args);
  }
  warn(...args) {
    this._write("stderr", args);
  }
  error(...args) {
    this._write("stderr", args);
  }
  trace(...args) {
    const error = new Error(formatWithOptions({}, ...args));
    error.name = "Trace";
    this._write("stderr", [`Trace: ${error.message}\n${(error.stack || "").trimEnd()}`]);
  }
  dir(value, options) {
    this._stdout.write(this._groupIndent + inspect(value, Object.assign({ customInspect: false }, this._inspectOptions, options)) + "\n");
  }
  dirxml(...args) {
    this.log(...args);
  }
  assert(condition, ...args) {
    if (condition) return;
    if (typeof args[0] === "string") args[0] = `Assertion failed: ${args[0]}`;
    else args.unshift("Assertion failed");
    this.warn(...args);
  }
  count(label = "default") {
    const count = (this._counts.get(label) || 0) + 1;
    this._counts.set(label, count);
    this.log(`${label}: ${count}`);
  }
  countReset(label = "default") {
    this._counts.delete(label);
  }
  group(...args) {
    if (args.length) this.log(...args);
    this._groupIndent += "  ";
  }
  groupCollapsed(...args) {
    this.group(...args);
  }
  groupEnd() {
    this._groupIndent = this._groupIndent.slice(0, -2);
  }
  time(label = "default") {
    if (this._timers.has(label)) {
      process.emitWarning(`Label '${label}' already exists for console.time()`);
      return;
    }
    this._timers.set(label, performance.now());
  }
  _elapsed(label) {
    const start = this._timers.get(label);
    if (start === undefined) {
      process.emitWarning(`No such label '${label}' for console.timeEnd()`);
      return undefined;
    }
    const ms = performance.now() - start;
    return ms >= 1000 ? `${(ms / 1000).toFixed(3)}s` : `${ms.toFixed(3)}ms`;
  }
  timeEnd(label = "default") {
    const elapsed = this._elapsed(label);
    this._timers.delete(label);
    if (elapsed !== undefined) this.log(`${label}: ${elapsed}`);
  }
  timeLog(label = "default", ...data) {
    const elapsed = this._elapsed(label);
    if (elapsed !== undefined) this.log(`${label}: ${elapsed}`, ...data);
  }
  table(data, properties) {
    if (data === null || typeof data !== "object") {
      this.log(data);
      return;
    }
    const rowKeys = data instanceof Map ? Array.from(data.keys()) : Object.keys(data);
    const values = data instanceof Map ? Array.from(data.values()) : rowKeys.map((key) => data[key]);
    const columns = [];
    let hasValues = false;
    for (const value of values) {
      if (value !== null && typeof value === "object") {
        for (const key of Object.keys(value)) if (!columns.includes(key)) columns.push(key);
      } else {
        hasValues = true;
      }
    }
    const shown = properties || columns;
    const head = ["(index)", ...shown, ...(hasValues ? ["Values"] : [])];
    const rows = values.map((value, index) => {
      const isObject = value !== null && typeof value === "object";
      const cells = shown.map((key) => (isObject && key in value ? cellText(value[key]) : ""));
      if (hasValues) cells.push(isObject ? "" : cellText(value));
      return [String(rowKeys[index]), ...cells];
    });
    this._stdout.write(renderTable(head, rows) + "\n");
  }
  clear() {
    if (this._stdout.isTTY) this._stdout.write("\u001b[1;1H\u001b[0J");
  }
  profile() {}
  profileEnd() {}
  timeStamp() {}
}

const console = new Console({ stdout: nativeStream("stdout"), stderr: nativeStream("stderr") });
console.Console = Console;
module.exports = console;
