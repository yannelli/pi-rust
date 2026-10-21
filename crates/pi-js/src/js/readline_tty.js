// internal/readline_tty: terminal line editing for readline.Interface (cursor keys, home and
// end, deletion, words, and history), ported from Node's internal/readline/interface.js.
const { emitKeypressEvents, charLengthAt, charLengthLeft } = require("internal/readline_keys");

const LINE_ENDING = /\r?\n|\r(?!\n)/g;

function isFullWidth(code) {
  return code >= 0x1100 && (code <= 0x115f || code === 0x2329 || code === 0x232a ||
    (code >= 0x2e80 && code <= 0x3247 && code !== 0x303f) || (code >= 0x3250 && code <= 0x4dbf) ||
    (code >= 0x4e00 && code <= 0xa4c6) || (code >= 0xa960 && code <= 0xa97c) || (code >= 0xac00 && code <= 0xd7a3) ||
    (code >= 0xf900 && code <= 0xfaff) || (code >= 0xfe10 && code <= 0xfe19) || (code >= 0xfe30 && code <= 0xfe6b) ||
    (code >= 0xff01 && code <= 0xff60) || (code >= 0xffe0 && code <= 0xffe6) || (code >= 0x1b000 && code <= 0x1b001) ||
    (code >= 0x1f200 && code <= 0x1f251) || (code >= 0x1f300 && code <= 0x1f64f) || (code >= 0x20000 && code <= 0x3fffd));
}

function isZeroWidth(code) {
  return code <= 0x1f || (code >= 0x7f && code <= 0x9f) || (code >= 0x300 && code <= 0x36f) ||
    (code >= 0x200b && code <= 0x200f) || (code >= 0x20d0 && code <= 0x20ff) || (code >= 0xfe00 && code <= 0xfe0f) ||
    (code >= 0xfe20 && code <= 0xfe2f) || (code >= 0xe0100 && code <= 0xe01ef);
}

function charWidth(char) {
  const code = char.codePointAt(0);
  if (isZeroWidth(code)) return 0;
  return isFullWidth(code) ? 2 : 1;
}

const output = () => require("readline");

const methods = {
  get columns() {
    return this.output && this.output.columns ? this.output.columns : Infinity;
  },
  _writeToOutput(text) {
    if (this.output !== null && this.output !== undefined) this.output.write(text);
  },
  _setRawMode(mode) {
    const wasRaw = this.input.isRaw;
    if (typeof this.input.setRawMode === "function") this.input.setRawMode(mode);
    return wasRaw;
  },
  _getDisplayPos(str) {
    let offset = 0;
    let rows = 0;
    const col = this.columns;
    for (const char of require("util").stripVTControlCharacters(str)) {
      if (char === "\n") {
        rows += Math.ceil(offset / col) || 1;
        offset = 0;
      } else if (char === "\t") {
        offset += this.tabSize - (offset % this.tabSize);
      } else {
        const width = charWidth(char);
        if (width === 2 && (offset + 1) % col === 0) offset++;
        offset += width;
      }
    }
    const cols = offset % col;
    rows += (offset - cols) / col;
    return { cols, rows };
  },
  getCursorPos() {
    return this._getDisplayPos(this._prompt + this.line.slice(0, this.cursor));
  },
  _refreshLine() {
    const { cursorTo, moveCursor, clearScreenDown } = output();
    const line = this._prompt + this.line;
    const display = this._getDisplayPos(line);
    const cursor = this.getCursorPos();
    if (this.prevRows > 0) moveCursor(this.output, 0, -this.prevRows);
    cursorTo(this.output, 0);
    clearScreenDown(this.output);
    this._writeToOutput(line);
    if (display.cols === 0) this._writeToOutput(" ");
    cursorTo(this.output, cursor.cols);
    const diff = display.rows - cursor.rows;
    if (diff > 0) moveCursor(this.output, 0, -diff);
    this.prevRows = cursor.rows;
  },
  _insertString(text) {
    if (this.cursor < this.line.length) {
      this.line = this.line.slice(0, this.cursor) + text + this.line.slice(this.cursor);
      this.cursor += text.length;
      if (this.isCompletionEnabled === false) this._writeToOutput(text);
      else this._refreshLine();
      return;
    }
    const before = this.getCursorPos();
    this.line += text;
    this.cursor += text.length;
    if (this.isCompletionEnabled !== false && before.rows < this.getCursorPos().rows) this._refreshLine();
    else this._writeToOutput(text);
  },
  _moveCursor(dx) {
    if (dx === 0) return;
    const before = this.getCursorPos();
    this.cursor = Math.min(Math.max(this.cursor + dx, 0), this.line.length);
    const after = this.getCursorPos();
    if (before.rows === after.rows) output().moveCursor(this.output, after.cols - before.cols, 0);
    else this._refreshLine();
  },
  _wordLeft() {
    if (this.cursor === 0) return;
    const reversed = Array.from(this.line.slice(0, this.cursor)).reverse().join("");
    this._moveCursor(-/^\s*(?:[^\w\s]+|\w+)?/.exec(reversed)[0].length);
  },
  _wordRight() {
    if (this.cursor >= this.line.length) return;
    this._moveCursor(/^(?:\s+|[^\w\s]+|\w+)\s*/.exec(this.line.slice(this.cursor))[0].length);
  },
  _deleteLeft() {
    if (this.cursor === 0 || this.line.length === 0) return;
    const size = charLengthLeft(this.line, this.cursor);
    this.line = this.line.slice(0, this.cursor - size) + this.line.slice(this.cursor);
    this.cursor -= size;
    this._refreshLine();
  },
  _deleteRight() {
    if (this.cursor >= this.line.length) return;
    this.line = this.line.slice(0, this.cursor) + this.line.slice(this.cursor + charLengthAt(this.line, this.cursor));
    this._refreshLine();
  },
  _deleteWordLeft() {
    if (this.cursor === 0) return;
    let leading = this.line.slice(0, this.cursor);
    const reversed = Array.from(leading).reverse().join("");
    leading = leading.slice(0, leading.length - /^\s*(?:[^\w\s]+|\w+)?/.exec(reversed)[0].length);
    this.line = leading + this.line.slice(this.cursor);
    this.cursor = leading.length;
    this._refreshLine();
  },
  _deleteWordRight() {
    if (this.cursor >= this.line.length) return;
    const trailing = this.line.slice(this.cursor);
    this.line = this.line.slice(0, this.cursor) + trailing.slice(/^(?:\s+|\W+|\w+)\s*/.exec(trailing)[0].length);
    this._refreshLine();
  },
  _deleteLineLeft() {
    this.line = this.line.slice(this.cursor);
    this.cursor = 0;
    this._refreshLine();
  },
  _deleteLineRight() {
    this.line = this.line.slice(0, this.cursor);
    this._refreshLine();
  },
  clearLine() {
    this._moveCursor(Infinity);
    this._writeToOutput("\r\n");
    this.line = "";
    this.cursor = 0;
    this.prevRows = 0;
  },
  _addHistory() {
    if (this.line.length === 0) return "";
    if (this.historySize === 0 || this.line.trim().length === 0) return this.line;
    if (this.history.length === 0 || this.history[0] !== this.line) {
      if (this.removeHistoryDuplicates) {
        const index = this.history.indexOf(this.line);
        if (index !== -1) this.history.splice(index, 1);
      }
      this.history.unshift(this.line);
      if (this.history.length > this.historySize) this.history.pop();
    }
    this.historyIndex = -1;
    const line = this.history[0];
    this.emit("history", this.history);
    return line;
  },
  _line() {
    const line = this._addHistory();
    this.clearLine();
    this._onLine(line);
  },
  _historyStep(step) {
    const search = this._substringSearch || "";
    let index = this.historyIndex + step;
    while (index >= 0 && index < this.history.length && (!this.history[index].startsWith(search) || this.line === this.history[index])) {
      index += step;
    }
    this.line = index === -1 || index === this.history.length ? search : this.history[index];
    this.historyIndex = index;
    this.cursor = this.line.length;
    this._refreshLine();
  },
  _historyNext() {
    if (this.historyIndex >= 0) this._historyStep(-1);
  },
  _historyPrev() {
    if (this.historyIndex < this.history.length && this.history.length) this._historyStep(1);
  },
  _ttyWrite(s, key) {
    key = key || {};
    if ((key.name === "up" || key.name === "down") && !key.ctrl && !key.meta && !key.shift) {
      if (this._substringSearch === null) this._substringSearch = this.line.slice(0, this.cursor);
    } else if (this._substringSearch !== null) {
      this._substringSearch = null;
      if (this.history.length === this.historyIndex) this.historyIndex = -1;
    }
    if (key.name === "escape") return;
    if (key.ctrl && key.shift) {
      if (key.name === "backspace") this._deleteLineLeft();
      else if (key.name === "delete") this._deleteLineRight();
    } else if (key.ctrl) {
      const actions = {
        c: () => (this.listenerCount("SIGINT") > 0 ? this.emit("SIGINT") : this.close()),
        h: () => this._deleteLeft(),
        d: () => {
          if (this.cursor === 0 && this.line.length === 0) this.close();
          else if (this.cursor < this.line.length) this._deleteRight();
        },
        u: () => this._deleteLineLeft(),
        k: () => this._deleteLineRight(),
        a: () => this._moveCursor(-Infinity),
        e: () => this._moveCursor(Infinity),
        b: () => this._moveCursor(-charLengthLeft(this.line, this.cursor)),
        f: () => this._moveCursor(charLengthAt(this.line, this.cursor)),
        l: () => {
          output().cursorTo(this.output, 0, 0);
          output().clearScreenDown(this.output);
          this._refreshLine();
        },
        n: () => this._historyNext(),
        p: () => this._historyPrev(),
        w: () => this._deleteWordLeft(),
        backspace: () => this._deleteWordLeft(),
        delete: () => this._deleteWordRight(),
        left: () => this._wordLeft(),
        right: () => this._wordRight(),
      };
      if (Object.hasOwn(actions, key.name)) actions[key.name]();
    } else if (key.meta) {
      const actions = { b: "_wordLeft", f: "_wordRight", d: "_deleteWordRight", delete: "_deleteWordRight", backspace: "_deleteWordLeft" };
      if (Object.hasOwn(actions, key.name)) this[actions[key.name]]();
    } else {
      if (this._sawReturnAt && key.name !== "enter") this._sawReturnAt = 0;
      const actions = {
        return: () => {
          this._sawReturnAt = Date.now();
          this._line();
        },
        enter: () => {
          if (this._sawReturnAt === 0 || Date.now() - this._sawReturnAt > this.crlfDelay) this._line();
          this._sawReturnAt = 0;
        },
        backspace: () => this._deleteLeft(),
        delete: () => this._deleteRight(),
        left: () => this._moveCursor(-charLengthLeft(this.line, this.cursor)),
        right: () => this._moveCursor(charLengthAt(this.line, this.cursor)),
        home: () => this._moveCursor(-Infinity),
        end: () => this._moveCursor(Infinity),
        up: () => this._historyPrev(),
        down: () => this._historyNext(),
      };
      if (Object.hasOwn(actions, key.name)) {
        actions[key.name]();
      } else if (typeof s === "string" && s) {
        LINE_ENDING.lastIndex = 0;
        let lastIndex = 0;
        let match;
        while ((match = LINE_ENDING.exec(s)) !== null) {
          this._insertString(s.slice(lastIndex, match.index));
          ({ lastIndex } = LINE_ENDING);
          this._line();
          LINE_ENDING.lastIndex = lastIndex;
        }
        this._insertString(s.slice(lastIndex));
      }
    }
  },
};

// Switch `rl` to terminal mode: raw input, keypress events, and an editable line.
function attach(rl) {
  const { input } = rl;
  const onkeypress = (s, key) => {
    rl._ttyWrite(s, key);
    if (key && key.sequence) {
      const code = key.sequence.codePointAt(0);
      if (code >= 0xd800 && code <= 0xdfff) rl._refreshLine();
    }
  };
  const onend = () => {
    if (typeof rl.line === "string" && rl.line.length > 0) rl.emit("line", rl.line);
    rl.close();
  };
  const onresize = () => rl._refreshLine();
  emitKeypressEvents(input, rl);
  input.on("keypress", onkeypress);
  input.on("end", onend);
  rl._setRawMode(true);
  rl.cursor = 0;
  rl.historyIndex = -1;
  rl._substringSearch = null;
  rl._sawReturnAt = 0;
  if (rl.output !== null && rl.output !== undefined && typeof rl.output.on === "function") rl.output.on("resize", onresize);
  rl.once("close", () => {
    input.removeListener("keypress", onkeypress);
    input.removeListener("end", onend);
    if (rl.output !== null && rl.output !== undefined && typeof rl.output.removeListener === "function") rl.output.removeListener("resize", onresize);
  });
  if (typeof input.resume === "function") input.resume();
}

function install(Interface) {
  for (const name of Object.getOwnPropertyNames(methods)) {
    Object.defineProperty(Interface.prototype, name, Object.getOwnPropertyDescriptor(methods, name));
  }
}

module.exports = { attach, install };
