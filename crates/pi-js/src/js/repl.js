// node:repl: a read-eval-print loop over any input and output streams. Code runs in a vm context
// (or the main context with useGlobal), multi-line input waits for more lines while the code is
// incomplete, and dot commands work as in Node.
const fs = require("fs");
const path = require("path");
const vm = require("vm");
const { Interface } = require("readline");
const { inspect } = require("util");
const { codeError } = require("internal/errors");

const REPL_MODE_SLOPPY = Symbol("repl-sloppy");
const REPL_MODE_STRICT = Symbol("repl-strict");
const EOF_MESSAGES = /end of input|unexpected end of|unterminated|: ''$/i;
let evaluations = 0;

class Recoverable extends SyntaxError {
  constructor(err) {
    super();
    this.err = err;
  }
}

function writer(object) {
  return inspect(object, writer.options);
}
writer.options = { ...inspect.defaultOptions, showProxy: true };

// Whether `code` stops inside a string, template, comment, or open bracket.
function unbalanced(code) {
  const stack = [];
  for (let i = 0; i < code.length; i++) {
    const c = code[i];
    if (c === "/" && code[i + 1] === "/") {
      while (i < code.length && code[i] !== "\n") i++;
    } else if (c === "/" && code[i + 1] === "*") {
      const end = code.indexOf("*/", i + 2);
      if (end < 0) return true;
      i = end + 1;
    } else if (c === "'" || c === '"' || c === "`") {
      let j = i + 1;
      while (j < code.length && code[j] !== c) {
        if (code[j] === "\\") j++;
        else if (c !== "`" && code[j] === "\n") return false;
        j++;
      }
      if (j >= code.length) return true;
      i = j;
    } else if ("([{".includes(c)) {
      stack.push(c);
    } else if (")]}".includes(c)) {
      if (stack.pop() !== "([{"[")]}".indexOf(c)]) return false;
    }
  }
  return stack.length > 0;
}

function recoverable(error, code) {
  return error instanceof SyntaxError && (EOF_MESSAGES.test(error.message) || unbalanced(code));
}

const DECLARATION = /^\s*(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*([\s\S]*?);?\s*$/;

// Top-level await: run the code in an async function; a single declaration stays visible.
function awaitScript(code) {
  const declaration = DECLARATION.exec(code);
  if (declaration) return { name: declaration[1], source: `(async () => await (${declaration[2]}\n))()` };
  for (const source of [`(async () => { return (${code.trim().replace(/;$/, "")}\n); })()`, `(async () => { ${code}\n })()`]) {
    try {
      new vm.Script(source);
      return { source };
    } catch {}
  }
  return null;
}

function defaultEval(code, context, file, callback) {
  const run = (source) => {
    const script = new vm.Script(source, { filename: file });
    return this.useGlobal ? script.runInThisContext() : script.runInContext(context);
  };
  let source = code;
  if (/^\s*{/.test(code) && !/;\s*$/.test(code)) {
    try {
      new vm.Script(`(${code.trim()})\n`);
      source = `(${code.trim()})\n`;
    } catch {}
  }
  if (this.replMode === REPL_MODE_STRICT && source.trim()) source = `'use strict'; void 0;\n${source}`;
  let result;
  try {
    result = run(source);
  } catch (error) {
    const wrapped = error instanceof SyntaxError && /\bawait\b/.test(code) ? awaitScript(code) : null;
    if (!wrapped) return callback(recoverable(error, code) ? new Recoverable(error) : error);
    try {
      if (wrapped.name) run(`var ${wrapped.name};`);
      const promise = run(wrapped.source);
      const settle = (value) => {
        if (wrapped.name === undefined) return callback(null, value);
        context[wrapped.name] = value;
        callback(null, undefined);
      };
      return promise.then(settle, (thrown) => callback(thrown));
    } catch (thrown) {
      return callback(thrown);
    }
  }
  callback(null, result);
}

// Error text as Node's REPL prints it: without the frames of the REPL's own evaluation.
function describeError(server, error) {
  let text;
  if (Object.prototype.toString.call(error) === "[object Error]" && typeof error.stack === "string") {
    const header = error.message ? `${error.name}: ${error.message}` : String(error.name);
    const frames = [];
    for (const line of error.stack.split("\n")) {
      if (/^\s+at /.test(line)) {
        if (/^\s+at (?:<eval>|<anonymous>) \(REPL\d+:|^\s+at REPL\d+:|node:/.test(line)) break;
        frames.push(line);
      }
    }
    const saved = Object.getOwnPropertyDescriptor(error, "stack");
    Object.defineProperty(error, "stack", { value: [header, ...frames].join("\n"), configurable: true, writable: true });
    try {
      text = server.writer(error);
    } finally {
      if (saved) Object.defineProperty(error, "stack", saved);
    }
  } else {
    text = server.writer(error);
  }
  const lines = text.split(/(?<=\n)/);
  let matched = false;
  text = lines.map((line) => {
    if (matched || !/^\[?([A-Z][a-z0-9_]*)*Error/.test(line)) return line;
    matched = true;
    return `Uncaught ${line}`;
  }).join("");
  if (!matched) text = `Uncaught${lines.length === 1 ? " " : ":\n"}${text}`;
  return text.endsWith("\n") ? text : `${text}\n`;
}

class REPLServer extends Interface {
  // Lines that arrive while an asynchronous evaluation runs wait for it to finish.
  #pending = false;
  #queued = [];
  #closeAfter = false;

  constructor(prompt, stream, evaluate, useGlobal, ignoreUndefined, replMode) {
    const options = prompt !== null && typeof prompt === "object"
      ? { ...prompt }
      : { prompt, stream, eval: evaluate, useGlobal, ignoreUndefined, replMode };
    const input = options.input ?? options.stream ?? process.stdin;
    const output = options.output ?? options.stream ?? process.stdout;
    super({ input, output, prompt: options.prompt ?? "> ", terminal: options.terminal });
    this._initialPrompt = options.prompt ?? "> ";
    this.useGlobal = !!options.useGlobal;
    this.ignoreUndefined = !!options.ignoreUndefined;
    this.replMode = options.replMode ?? REPL_MODE_SLOPPY;
    this.useColors = !!options.useColors;
    this.writer = options.writer ?? (this.useColors ? (value) => inspect(value, { ...writer.options, colors: true }) : writer);
    this.eval = options.eval ?? defaultEval;
    this.breakEvalOnSigint = !!options.breakEvalOnSigint;
    this.editorMode = false;
    this.commands = Object.create(null);
    this.bufferedCommand = "";
    this.last = undefined;
    this.lastError = undefined;
    defineDefaultCommands(this);
    this.resetContext();
    this.on("line", (line) => {
      if (this.#pending) this.#queued.push(line);
      else this.#onLine(line);
    });
    this.once("close", () => this.emit("exit"));
    this.displayPrompt();
  }

  createContext() {
    if (this.useGlobal) return globalThis;
    const context = vm.createContext();
    const own = new Set(vm.runInContext("Object.getOwnPropertyNames(globalThis)", context));
    for (const name of Object.getOwnPropertyNames(globalThis)) {
      if (own.has(name) || name === "global") continue;
      Object.defineProperty(context, name, Object.getOwnPropertyDescriptor(globalThis, name));
    }
    context.global = context;
    const filename = path.join(process.cwd(), "<repl>");
    context.module = { id: "<repl>", filename: null, exports: {}, loaded: false, children: [], paths: [] };
    context.exports = context.module.exports;
    context.require = internal.createRequire(filename);
    for (const name of require("module").builtinModules) {
      if (name.includes("/") || name.startsWith("_") || name in context) continue;
      Object.defineProperty(context, name, {
        get() {
          const lib = require(name);
          Object.defineProperty(context, name, { value: lib, writable: true, configurable: true });
          return lib;
        },
        set(value) {
          Object.defineProperty(context, name, { value, writable: true, configurable: true });
        },
        configurable: true,
      });
    }
    return context;
  }

  resetContext() {
    this.context = this.createContext();
    this.underscoreAssigned = false;
    this.lines = [];
    this.lines.level = [];
    for (const [name, field] of [["_", "last"], ["_error", "lastError"]]) {
      Object.defineProperty(this.context, name, {
        get: () => this[field],
        set: (value) => {
          this[field] = value;
          if (name === "_" && !this.underscoreAssigned) {
            this.underscoreAssigned = true;
            this.output.write("Expression assignment to _ now disabled.\n");
          }
        },
        configurable: true,
      });
    }
    this.emit("reset", this.context);
  }

  #onLine(line) {
    const trimmed = (line || "").trim();
    if (this.editorMode) {
      this.bufferedCommand += `${line}\n`;
      this.lines.push(line);
      return;
    }
    if (trimmed[0] === "." && trimmed[1] !== "." && Number.isNaN(Number.parseFloat(trimmed))) {
      const match = /^\.([^\s]+)\s*(.*)$/.exec(trimmed);
      const command = match && this.commands[match[1]];
      if (command) {
        command.action.call(this, match[2]);
        return;
      }
      if (!this.bufferedCommand) {
        this.output.write("Invalid REPL keyword\n");
        this.displayPrompt();
        return;
      }
    }
    if (!trimmed && !this.bufferedCommand) {
      this.displayPrompt();
      return;
    }
    this.lines.push(line);
    const code = `${this.bufferedCommand}${line}\n`;
    let sync = true;
    const finish = (error, result) => {
      this.#pending = false;
      this.#report(line, error, result);
      if (!sync) this.#drain();
    };
    this.#pending = true;
    try {
      this.eval(code, this.context, `REPL${++evaluations}`, finish);
    } catch (error) {
      finish(error);
    }
    sync = false;
  }

  #drain() {
    while (!this.#pending && this.#queued.length > 0 && !this.closed) this.#onLine(this.#queued.shift());
    if (!this.#pending && this.#closeAfter) {
      this.#closeAfter = false;
      this.close();
    }
  }

  #report(line, error, result) {
    if (error instanceof Recoverable) {
      this.bufferedCommand += `${line}\n`;
      this.displayPrompt();
      return;
    }
    this.clearBufferedCommand();
    if (error !== null && error !== undefined) {
      this.lastError = error;
      this.output.write(describeError(this, error));
      this.lines.level = [];
      this.displayPrompt();
      return;
    }
    if (!this.ignoreUndefined || result !== undefined) {
      if (!this.underscoreAssigned) this.last = result;
      this.output.write(`${this.writer(result)}\n`);
    }
    this.displayPrompt();
  }

  close() {
    if (this.#pending) {
      this.#closeAfter = true;
      return;
    }
    this.#queued = [];
    super.close();
  }

  clearBufferedCommand() {
    this.bufferedCommand = "";
  }

  displayPrompt() {
    if (this.closed) return;
    this.setPrompt(this.bufferedCommand.length ? `...${" ".repeat(this.lines.level.length * 2)} ` : this._initialPrompt);
    this.prompt();
  }

  defineCommand(keyword, command) {
    if (typeof command === "function") command = { action: command };
    if (typeof command?.action !== "function") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "cmd.action" property must be of type function. Received ${inspect(command?.action)}`);
    }
    this.commands[keyword] = command;
  }

  setupHistory(historyPath, callback) {
    if (typeof callback === "function") process.nextTick(callback, null, this);
  }
}

function defineDefaultCommands(server) {
  server.defineCommand("break", {
    help: "Sometimes you get stuck, this gets you out",
    action() {
      this.clearBufferedCommand();
      this.displayPrompt();
    },
  });
  server.defineCommand("clear", {
    help: server.useGlobal ? "Alias for .break" : "Break, and also clear the local context",
    action() {
      this.clearBufferedCommand();
      if (!this.useGlobal) {
        this.output.write("Clearing context...\n");
        this.resetContext();
      }
      this.displayPrompt();
    },
  });
  server.defineCommand("exit", { help: "Exit the REPL", action() { this.close(); } });
  server.defineCommand("help", {
    help: "Print this help message",
    action() {
      const names = Object.keys(this.commands).sort();
      const longest = Math.max(...names.map((name) => name.length));
      for (const name of names) {
        const help = this.commands[name].help;
        this.output.write(`.${name}${help ? `${" ".repeat(longest - name.length + 3)}${help}` : ""}\n`);
      }
      this.output.write("\nPress Ctrl+C to abort current expression, Ctrl+D to exit the REPL\n");
      this.displayPrompt();
    },
  });
  server.defineCommand("save", {
    help: "Save all evaluated commands in this REPL session to a file",
    action(file) {
      if (!file) this.output.write('The "file" argument must be specified\n');
      else {
        try {
          fs.writeFileSync(file, this.lines.join("\n"));
          this.output.write(`Session saved to: ${file}\n`);
        } catch {
          this.output.write(`Failed to save: ${file}\n`);
        }
      }
      this.displayPrompt();
    },
  });
  server.defineCommand("load", {
    help: "Load JS from a file into the REPL session",
    action(file) {
      try {
        if (!file) throw new Error("missing");
        if (fs.statSync(file).isFile()) {
          const data = fs.readFileSync(file, "utf8");
          this.editorMode = true;
          this.write(data);
          this.editorMode = false;
          this.write("\n");
        } else {
          this.output.write(`Failed to load: ${file} is not a valid file\n`);
        }
      } catch {
        this.output.write(`Failed to load: ${file}\n`);
      }
      this.displayPrompt();
    },
  });
}

function start(prompt, source, evaluate, useGlobal, ignoreUndefined, replMode) {
  return new REPLServer(prompt, source, evaluate, useGlobal, ignoreUndefined, replMode);
}

module.exports = {
  start,
  writer,
  REPLServer,
  REPL_MODE_SLOPPY,
  REPL_MODE_STRICT,
  Recoverable,
  get builtinModules() {
    return require("module").builtinModules.filter((name) => !name.startsWith("_"));
  },
};
