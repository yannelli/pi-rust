// node:readline: line splitting over any readable stream, questions, and cursor helpers.
// Terminal mode (line editing over raw input) is in internal/readline_tty.
const EventEmitter = require("events");
const { StringDecoder } = require("string_decoder");
const tty = require("internal/readline_tty");
const { validate } = require("internal/errors");

const LINE_BREAK = /\r\n|\n|\r/;

class Interface extends EventEmitter {
  #decoder = new StringDecoder("utf8");
  #pending = "";
  #questions = [];
  #closed = false;
  #paused = false;
  #sawReturn = false;
  #oldPrompt = null;
  constructor(input, output, completer, terminal) {
    super();
    const options = input && typeof input.on === "function" ? { input, output, completer, terminal } : input || {};
    this.input = options.input;
    this.output = options.output;
    this.line = "";
    this.cursor = 0;
    if (options.history !== undefined) validate.array(options.history, "history");
    this.history = options.history === undefined ? [] : options.history;
    const historySize = options.historySize === undefined ? 30 : options.historySize;
    if (typeof historySize !== "number") throw require("internal/errors").invalidArgType("historySize", "of type number", historySize);
    if (Number.isNaN(historySize) || historySize < 0) throw require("internal/errors").outOfRange("historySize", ">= 0", historySize);
    this.historySize = historySize;
    this.removeHistoryDuplicates = !!options.removeHistoryDuplicates;
    this.completer = options.completer;
    this.tabSize = options.tabSize === undefined ? 8 : options.tabSize;
    this.escapeCodeTimeout = options.escapeCodeTimeout === undefined ? 500 : options.escapeCodeTimeout;
    this.isCompletionEnabled = true;
    this.crlfDelay = Math.max(100, options.crlfDelay || 100);
    this._prompt = options.prompt === undefined ? "> " : String(options.prompt);
    let isTerminal = options.terminal;
    if (isTerminal === undefined && this.output !== null && this.output !== undefined) isTerminal = !!this.output.isTTY;
    this.terminal = !!isTerminal && !!this.input;
    if (options.signal) {
      if (options.signal.aborted) process.nextTick(() => this.close());
      else options.signal.addEventListener("abort", () => this.close(), { once: true });
    }
    if (this.terminal) {
      tty.attach(this);
    } else if (this.input) {
      this._ondata = (chunk) => this.#receive(typeof chunk === "string" ? chunk : this.#decoder.write(chunk));
      this._onend = () => {
        this.#receive(this.#decoder.end());
        if (this.#pending.length > 0) this.#emitLine(this.#pending);
        this.#pending = "";
        this.close();
      };
      this.input.on("data", this._ondata);
      this.input.on("end", this._onend);
      if (typeof this.input.resume === "function") this.input.resume();
    }
    if (process.env.TERM === "dumb") this._ttyWrite = tty.ttyWriteDumb.bind(this);
  }
  #receive(text) {
    if (!text) return;
    if (this.#sawReturn && text.startsWith("\n")) text = text.slice(1);
    this.#sawReturn = false;
    const parts = (this.#pending + text).split(LINE_BREAK);
    this.#pending = parts.pop();
    if (text.endsWith("\r")) this.#sawReturn = true;
    for (const line of parts) this.#emitLine(line);
  }
  #emitLine(line) {
    if (this.#closed) return;
    this.line = "";
    const question = this.#questions.shift();
    if (question && this.#questions.length === 0 && this.#oldPrompt !== null) {
      this._prompt = this.#oldPrompt;
      this.#oldPrompt = null;
    }
    if (question) question(line);
    else this.emit("line", line);
  }
  _onLine(line) {
    this.#emitLine(line);
  }
  get closed() {
    return this.#closed;
  }
  setPrompt(prompt) {
    this._prompt = String(prompt);
  }
  getPrompt() {
    return this._prompt;
  }
  prompt(preserveCursor) {
    if (this.#closed) throw Object.assign(new Error("readline was closed"), { code: "ERR_USE_AFTER_CLOSE" });
    if (this.#paused) this.resume();
    if (this.terminal && process.env.TERM !== "dumb") {
      if (!preserveCursor) this.cursor = 0;
      this._refreshLine();
    } else if (this.output && typeof this.output.write === "function") {
      this.output.write(this._prompt);
    }
  }
  question(query, options, callback) {
    if (typeof options === "function") [callback, options] = [options, {}];
    if (this.#closed) throw Object.assign(new Error("readline was closed"), { code: "ERR_USE_AFTER_CLOSE" });
    if (typeof callback !== "function") return;
    const signal = options && options.signal;
    if (signal && signal.aborted) return;
    let answered = false;
    const answer = (line) => {
      answered = true;
      callback(line);
    };
    this.#questions.push(answer);
    if (signal) {
      signal.addEventListener("abort", () => {
        if (!answered) this.#questions = this.#questions.filter((entry) => entry !== answer);
      }, { once: true });
    }
    if (this.terminal) {
      if (this.#oldPrompt === null) this.#oldPrompt = this._prompt;
      this._prompt = String(query);
      this.prompt();
    } else if (this.output && typeof this.output.write === "function") {
      this.output.write(String(query));
    }
  }
  write(data, key) {
    if (this.#closed) throw Object.assign(new Error("readline was closed"), { code: "ERR_USE_AFTER_CLOSE" });
    if (this.#paused) this.resume();
    if (this.terminal) this._ttyWrite(data, key);
    else if (typeof data === "string") this.#receive(data);
  }
  pause() {
    if (this.#paused || this.#closed) return this;
    if (this.input && typeof this.input.pause === "function") this.input.pause();
    this.#paused = true;
    this.emit("pause");
    return this;
  }
  resume() {
    if (!this.#paused || this.#closed) return this;
    if (this.input && typeof this.input.resume === "function") this.input.resume();
    this.#paused = false;
    this.emit("resume");
    return this;
  }
  close() {
    if (this.#closed) return;
    this.pause();
    if (this.terminal) this._setRawMode(false);
    this.#closed = true;
    if (this.input && this._ondata) {
      this.input.removeListener("data", this._ondata);
      this.input.removeListener("end", this._onend);
    }
    this.emit("close");
  }
  [Symbol.asyncIterator]() {
    return lineIterator(this);
  }
}

function lineIterator(lineReader) {
  const lines = [];
  let wake = null;
  let done = false;
  const notify = () => {
    if (wake) {
      const resolve = wake;
      wake = null;
      resolve();
    }
  };
  lineReader.on("line", (line) => {
    lines.push(line);
    notify();
  });
  lineReader.on("close", () => {
    done = true;
    notify();
  });
  return {
    async next() {
      while (lines.length === 0 && !done) await new Promise((resolve) => (wake = resolve));
      if (lines.length) return { value: lines.shift(), done: false };
      return { value: undefined, done: true };
    },
    async return() {
      lineReader.close();
      return { value: undefined, done: true };
    },
    [Symbol.asyncIterator]() {
      return this;
    },
  };
}

tty.install(Interface);

function createInterface(input, output, completer, terminal) {
  return new Interface(input, output, completer, terminal);
}

function writeAndCallback(stream, data, callback) {
  if (!stream || typeof stream.write !== "function") {
    if (typeof callback === "function") process.nextTick(callback, null);
    return true;
  }
  return stream.write(data, callback);
}

function cursorTo(stream, x, y, callback) {
  if (typeof y === "function") [callback, y] = [y, undefined];
  if (Number.isNaN(x) || Number.isNaN(y)) throw Object.assign(new TypeError("Invalid cursor position"), { code: "ERR_INVALID_CURSOR_POS" });
  const data = typeof y !== "number" ? `\x1b[${x + 1}G` : `\x1b[${y + 1};${x + 1}H`;
  return writeAndCallback(stream, data, callback);
}

function moveCursor(stream, dx, dy, callback) {
  let data = "";
  if (dx < 0) data += `\x1b[${-dx}D`;
  else if (dx > 0) data += `\x1b[${dx}C`;
  if (dy < 0) data += `\x1b[${-dy}A`;
  else if (dy > 0) data += `\x1b[${dy}B`;
  return writeAndCallback(stream, data, callback);
}

function clearLine(stream, dir, callback) {
  const codes = { "-1": "\x1b[1K", 1: "\x1b[0K", 0: "\x1b[2K" };
  return writeAndCallback(stream, codes[dir] || codes[0], callback);
}

function clearScreenDown(stream, callback) {
  return writeAndCallback(stream, "\x1b[0J", callback);
}

module.exports = {
  Interface,
  createInterface,
  cursorTo,
  moveCursor,
  clearLine,
  clearScreenDown,
  emitKeypressEvents: require("internal/readline_keys").emitKeypressEvents,
};
