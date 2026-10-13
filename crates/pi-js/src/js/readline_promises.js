// node:readline/promises: the readline Interface with promise-returning question().
const readline = require("readline");

class Interface extends readline.Interface {
  question(query, options = {}) {
    return new Promise((resolve, reject) => {
      const signal = options.signal;
      if (signal && signal.aborted) {
        reject(Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR", cause: signal.reason }));
        return;
      }
      if (signal) {
        signal.addEventListener("abort", () => {
          reject(Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR", cause: signal.reason }));
        }, { once: true });
      }
      super.question(query, options, resolve);
    });
  }
}

// Collects cursor commands and writes them on commit(), like Node's readline.promises.Readline.
class Readline {
  #stream;
  #pending = "";
  #autoCommit;
  constructor(stream, options = {}) {
    this.#stream = stream;
    this.#autoCommit = !!options.autoCommit;
  }
  #queue(data) {
    if (this.#autoCommit) this.#stream.write(data);
    else this.#pending += data;
    return this;
  }
  cursorTo(x, y) {
    return this.#queue(typeof y === "number" ? `\x1b[${y + 1};${x + 1}H` : `\x1b[${x + 1}G`);
  }
  moveCursor(dx, dy) {
    let data = "";
    if (dx) data += dx < 0 ? `\x1b[${-dx}D` : `\x1b[${dx}C`;
    if (dy) data += dy < 0 ? `\x1b[${-dy}A` : `\x1b[${dy}B`;
    return this.#queue(data);
  }
  clearLine(dir) {
    const codes = { "-1": "\x1b[1K", 1: "\x1b[0K", 0: "\x1b[2K" };
    return this.#queue(codes[dir] || codes[0]);
  }
  clearScreenDown() {
    return this.#queue("\x1b[0J");
  }
  commit() {
    const data = this.#pending;
    this.#pending = "";
    return new Promise((resolve) => (data ? this.#stream.write(data, resolve) : resolve()));
  }
  rollback() {
    this.#pending = "";
    return this;
  }
}

module.exports = {
  Interface,
  Readline,
  createInterface: (input, output, completer, terminal) => new Interface(input, output, completer, terminal),
};
