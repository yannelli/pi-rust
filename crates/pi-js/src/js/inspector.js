// node:inspector: in-process sessions that speak the Chrome DevTools protocol for the parts that
// map onto QuickJS (Runtime, HeapProfiler, Schema, NodeRuntime). Debugger and Profiler need V8,
// so their methods fail as unknown methods do, and no DevTools server can be opened.
const EventEmitter = require("events");
const { codeError, validate } = require("internal/errors");

const CONTEXT_ID = 1;
const AVAILABLE = ["Runtime", "HeapProfiler", "Schema", "NodeRuntime"];
const sessions = new Set();
let consolePatched = null;
let nextSession = 1;
let nextException = 1;
let nextScript = 1;

class ProtocolError extends Error {
  constructor(code, message) {
    super(message);
    this.protocolCode = code;
  }
}

const invalidParams = () => new ProtocolError(-32602, "Invalid parameters");
const notFound = () => new ProtocolError(-32000, "Could not find object with given id");

function tagOf(value) {
  return Object.prototype.toString.call(value).slice(8, -1);
}

function subtypeOf(value) {
  if (Array.isArray(value)) return "array";
  if (ArrayBuffer.isView(value)) return value instanceof DataView ? "dataview" : "typedarray";
  const subtypes = { Map: "map", Set: "set", WeakMap: "weakmap", WeakSet: "weakset", Error: "error", Date: "date", RegExp: "regexp", Promise: "promise", ArrayBuffer: "arraybuffer", Generator: "generator" };
  return subtypes[tagOf(value)];
}

function classNameOf(value) {
  try {
    const constructor = Object.getPrototypeOf(value)?.constructor;
    if (typeof constructor === "function" && constructor.name) return constructor.name;
  } catch {}
  return "Object";
}

function describe(value, subtype, className) {
  switch (subtype) {
    case "array":
      return `Array(${value.length})`;
    case "typedarray":
      return `${className}(${value.length})`;
    case "map":
    case "set":
      return `${className}(${value.size})`;
    case "error": {
      const frames = typeof value.stack === "string" ? value.stack.split("\n").filter((line) => /^\s+at /.test(line)) : [];
      return [`${value.name}: ${value.message}`, ...frames].join("\n");
    }
    case "date":
      return String(value);
    case "regexp":
      return RegExp.prototype.toString.call(value);
    case "promise":
      return "Promise";
  }
  return className;
}

function byValue(value) {
  const unserializable = numberText(value);
  if (unserializable !== undefined) return { type: typeof value, unserializableValue: unserializable, description: unserializable };
  if (value === undefined) return { type: "undefined" };
  if (typeof value === "number") return { type: "number", value, description: String(value) };
  const text = JSON.stringify(value);
  return { type: typeof value === "function" ? "function" : typeof value, value: text === undefined ? undefined : JSON.parse(text) };
}

function numberText(value) {
  if (typeof value === "bigint") return `${value}n`;
  if (typeof value !== "number") return undefined;
  if (Object.is(value, -0)) return "-0";
  if (!Number.isFinite(value)) return String(value);
  return undefined;
}

class Session extends EventEmitter {
  #connected = false;
  #objects = new Map();
  #groups = new Map();
  #prefix = `${Date.now()}${nextSession++}`;
  #nextObject = 1;
  #enabled = new Set();
  #waiting = new Set();

  connect() {
    if (this.#connected) throw codeError(Error, "ERR_INSPECTOR_ALREADY_CONNECTED", "The inspector session is already connected");
    this.#connected = true;
    sessions.add(this);
  }

  connectToMainThread() {
    if (require("worker_threads").isMainThread) throw codeError(Error, "ERR_INSPECTOR_NOT_WORKER", "Current thread is not a worker");
    this.connect();
  }

  disconnect() {
    if (!this.#connected) return;
    this.#connected = false;
    this.#objects.clear();
    this.#groups.clear();
    this.#enabled.clear();
    sessions.delete(this);
    patchConsole();
    const waiting = [...this.#waiting];
    this.#waiting.clear();
    for (const entry of waiting) entry.reply(new ProtocolError(-32000, "Execution context was destroyed."));
  }

  post(method, params, callback) {
    validate.string(method, "method");
    if (callback === undefined && typeof params === "function") [callback, params] = [params, null];
    if (params !== null && params !== undefined) validate.object(params, "params");
    if (callback !== undefined) validate.function(callback, "callback");
    if (!this.#connected) throw codeError(Error, "ERR_INSPECTOR_NOT_CONNECTED", "Session is not connected");
    const reply = (error, result) => {
      if (!callback) return;
      if (error) callback(codeError(Error, "ERR_INSPECTOR_COMMAND", `Inspector error ${error.protocolCode}: ${error.message}`));
      else callback(null, result);
    };
    let result;
    try {
      result = this.#dispatch(method, params || {});
    } catch (error) {
      if (!(error instanceof ProtocolError)) throw error;
      return reply(error);
    }
    if (result instanceof Promise) {
      const entry = { reply };
      this.#waiting.add(entry);
      const settle = (error, value) => {
        if (!this.#waiting.delete(entry)) return;
        reply(error, value);
      };
      result.then((value) => settle(null, value), (error) => settle(error));
    } else {
      reply(null, result);
    }
  }

  notify(method, params) {
    const message = { method, params };
    this.emit("inspectorNotification", message);
    this.emit(method, message);
  }

  wantsConsole() {
    return this.#enabled.has("Runtime");
  }

  #dispatch(method, params) {
    const domain = method.split(".")[0];
    if (!AVAILABLE.includes(domain)) {
      if (["Debugger", "Profiler", "Console", "Network", "NodeTracing", "NodeWorker"].includes(domain)) {
        throw new ProtocolError(-32601, `'${method}' wasn't found (the ${domain} domain needs V8 and is not available in pi-rust)`);
      }
      throw new ProtocolError(-32601, `'${method}' wasn't found`);
    }
    const handler = this.#handlers()[method];
    if (!handler) throw new ProtocolError(-32601, `'${method}' wasn't found`);
    return handler(params);
  }

  #remote(value, options = {}) {
    if (options.returnByValue && typeof value !== "symbol" && typeof value !== "function") return byValue(value);
    const unserializable = numberText(value);
    if (unserializable !== undefined) return { type: typeof value, unserializableValue: unserializable, description: unserializable };
    switch (typeof value) {
      case "undefined":
        return { type: "undefined" };
      case "number":
        return { type: "number", value, description: String(value) };
      case "string":
      case "boolean":
        return { type: typeof value, value };
      case "symbol":
        return { type: "symbol", description: value.toString(), objectId: this.#keep(value, options.objectGroup) };
      case "function":
        return { type: "function", className: "Function", description: Function.prototype.toString.call(value), objectId: this.#keep(value, options.objectGroup) };
    }
    if (value === null) return { type: "object", subtype: "null", value: null };
    const subtype = subtypeOf(value);
    const className = subtype === "error" ? value.name || "Error" : classNameOf(value);
    const remote = { type: "object" };
    if (subtype) remote.subtype = subtype;
    Object.assign(remote, { className, description: describe(value, subtype, className), objectId: this.#keep(value, options.objectGroup) });
    if (options.generatePreview) remote.preview = this.#preview(value, remote);
    return remote;
  }

  #preview(value, remote) {
    const keys = Object.keys(value);
    const properties = keys.slice(0, 5).map((name) => {
      const entry = value[name];
      const type = entry === null ? "object" : typeof entry;
      const shown = typeof entry === "object" && entry !== null ? describe(entry, subtypeOf(entry), classNameOf(entry)) : String(entry);
      return { name, type, value: shown, ...(entry === null ? { subtype: "null" } : {}) };
    });
    const preview = { type: "object" };
    if (remote.subtype) preview.subtype = remote.subtype;
    return Object.assign(preview, { description: remote.description, overflow: keys.length > 5, properties });
  }

  #keep(value, group) {
    const id = `${this.#prefix}.${CONTEXT_ID}.${this.#nextObject++}`;
    this.#objects.set(id, value);
    if (group) {
      if (!this.#groups.has(group)) this.#groups.set(group, []);
      this.#groups.get(group).push(id);
    }
    return id;
  }

  #object(objectId) {
    if (typeof objectId !== "string") throw invalidParams();
    if (!this.#objects.has(objectId)) throw notFound();
    return this.#objects.get(objectId);
  }

  #exception(error, options) {
    const exception = this.#remote(error, { ...options, returnByValue: false });
    const location = /:(\d+):(\d+)\)?$/m.exec(typeof error?.stack === "string" ? error.stack : "");
    return {
      result: exception,
      exceptionDetails: {
        exceptionId: nextException++,
        text: "Uncaught",
        lineNumber: location ? Number(location[1]) - 1 : 0,
        columnNumber: location ? Number(location[2]) - 1 : 0,
        scriptId: String(nextScript++),
        exception,
      },
    };
  }

  #settle(run, options) {
    let value;
    try {
      value = run();
    } catch (error) {
      return this.#exception(error, options);
    }
    if (options.awaitPromise && value instanceof Promise) {
      return value.then((resolved) => ({ result: this.#remote(resolved, options) }), (error) => this.#exception(error, options));
    }
    return { result: this.#remote(value, options) };
  }

  #argument(argument = {}) {
    if ("objectId" in argument) return this.#object(argument.objectId);
    if ("unserializableValue" in argument) {
      const text = argument.unserializableValue;
      return text.endsWith("n") ? BigInt(text.slice(0, -1)) : text === "-0" ? -0 : Number(text);
    }
    return argument.value;
  }

  #properties(params) {
    const object = this.#object(params.objectId);
    if (object === null || (typeof object !== "object" && typeof object !== "function")) return { result: [] };
    const result = [];
    for (const key of Reflect.ownKeys(object)) {
      const descriptor = Object.getOwnPropertyDescriptor(object, key);
      const entry = { name: typeof key === "symbol" ? key.toString() : key };
      if ("value" in descriptor) {
        if (params.accessorPropertiesOnly) continue;
        Object.assign(entry, { value: this.#remote(descriptor.value, params), writable: descriptor.writable });
      } else {
        entry.get = this.#remote(descriptor.get, params);
        entry.set = this.#remote(descriptor.set, params);
      }
      Object.assign(entry, { configurable: descriptor.configurable, enumerable: descriptor.enumerable, isOwn: true });
      if (typeof key === "symbol") entry.symbol = this.#remote(key, params);
      result.push(entry);
    }
    const prototype = Object.getPrototypeOf(object);
    const internalProperties = prototype === null ? [] : [{ name: "[[Prototype]]", value: this.#remote(prototype, params) }];
    return { result, internalProperties };
  }

  #handlers() {
    const enable = (domain, extra = () => {}) => () => {
      this.#enabled.add(domain);
      extra();
      return {};
    };
    const disable = (domain) => () => {
      this.#enabled.delete(domain);
      patchConsole();
      return {};
    };
    return {
      "Runtime.enable": enable("Runtime", () => {
        const context = { id: CONTEXT_ID, origin: "", name: `node[${process.pid}]`, uniqueId: `${this.#prefix}.${CONTEXT_ID}`, auxData: { isDefault: true } };
        this.notify("Runtime.executionContextCreated", { context });
        patchConsole();
      }),
      "Runtime.disable": disable("Runtime"),
      "Runtime.evaluate": (params) => {
        if (typeof params.expression !== "string") throw invalidParams();
        return this.#settle(() => (0, eval)(params.expression), params);
      },
      "Runtime.callFunctionOn": (params) => {
        if (typeof params.functionDeclaration !== "string") throw invalidParams();
        if (params.objectId === undefined && params.executionContextId === undefined && params.uniqueContextId === undefined) {
          throw new ProtocolError(-32602, "Either objectId or executionContextId or uniqueContextId must be specified");
        }
        const target = params.objectId === undefined ? undefined : this.#object(params.objectId);
        const args = (params.arguments || []).map((argument) => this.#argument(argument));
        return this.#settle(() => (0, eval)(`(${params.functionDeclaration})`).apply(target, args), params);
      },
      "Runtime.getProperties": (params) => this.#properties(params),
      "Runtime.releaseObject": (params) => {
        this.#object(params.objectId);
        this.#objects.delete(params.objectId);
        return {};
      },
      "Runtime.releaseObjectGroup": (params) => {
        for (const id of this.#groups.get(params.objectGroup) || []) this.#objects.delete(id);
        this.#groups.delete(params.objectGroup);
        return {};
      },
      "Runtime.awaitPromise": (params) => {
        const promise = this.#object(params.promiseObjectId);
        if (!(promise instanceof Promise)) throw new ProtocolError(-32000, "Could not find promise with given id");
        return this.#settle(() => promise, { ...params, awaitPromise: true });
      },
      "Runtime.compileScript": (params) => {
        if (!this.#enabled.has("Runtime")) throw new ProtocolError(-32000, "Runtime agent is not enabled");
        if (typeof params.expression !== "string") throw invalidParams();
        try {
          new (require("vm").Script)(params.expression, { filename: params.sourceURL || "" });
        } catch (error) {
          return { exceptionDetails: this.#exception(error, {}).exceptionDetails };
        }
        return params.persistScript ? { scriptId: String(nextScript++) } : {};
      },
      "Runtime.getHeapUsage": () => {
        const usage = process.memoryUsage();
        return { usedSize: usage.heapUsed, totalSize: usage.heapTotal };
      },
      "Runtime.globalLexicalScopeNames": () => ({ names: [] }),
      "Runtime.runIfWaitingForDebugger": () => ({}),
      "Runtime.discardConsoleEntries": () => ({}),
      "Runtime.getIsolateId": () => ({ id: this.#prefix }),
      "HeapProfiler.enable": enable("HeapProfiler"),
      "HeapProfiler.disable": disable("HeapProfiler"),
      "HeapProfiler.collectGarbage": () => ({}),
      "HeapProfiler.takeHeapSnapshot": () => {
        require("v8");
        this.notify("HeapProfiler.addHeapSnapshotChunk", { chunk: internal.heapSnapshotText() });
        return {};
      },
      "Schema.getDomains": () => ({ domains: AVAILABLE.slice(0, 3).map((name) => ({ name, version: "1.3" })) }),
      "NodeRuntime.enable": () => ({}),
      "NodeRuntime.disable": () => ({}),
      "NodeRuntime.notifyWhenWaitingForDisconnect": () => ({}),
    };
  }
}

// Console calls reach sessions with the Runtime domain on as Runtime.consoleAPICalled.
const CONSOLE_TYPES = { log: "log", info: "info", debug: "debug", warn: "warning", error: "error", dir: "dir", table: "table", trace: "trace" };

let sending = false;
function sendConsole(type, args) {
  // Console calls made by notification listeners are not reported again.
  if (sending) return;
  sending = true;
  try {
    for (const session of sessions) {
      if (!session.wantsConsole()) continue;
      session.notify("Runtime.consoleAPICalled", {
        type,
        args: args.map((arg) => (typeof arg === "object" && arg !== null ? { type: "object", description: classNameOf(arg) } : byValue(arg))),
        executionContextId: CONTEXT_ID,
        timestamp: Date.now(),
      });
    }
  } finally {
    sending = false;
  }
}

function patchConsole() {
  const wanted = [...sessions].some((session) => session.wantsConsole());
  if (wanted && !consolePatched) {
    consolePatched = {};
    for (const [name, type] of Object.entries(CONSOLE_TYPES)) {
      const original = console[name];
      consolePatched[name] = original;
      console[name] = function (...args) {
        sendConsole(type, args);
        return original.apply(this, args);
      };
    }
  } else if (!wanted && consolePatched) {
    for (const [name, original] of Object.entries(consolePatched)) console[name] = original;
    consolePatched = null;
  }
}

const inspectorConsole = {};
for (const [name, type] of Object.entries(CONSOLE_TYPES)) inspectorConsole[name] = (...args) => sendConsole(type, args);
for (const name of ["assert", "clear", "count", "countReset", "group", "groupCollapsed", "groupEnd", "time", "timeEnd", "timeLog", "dirxml", "profile", "profileEnd", "timeStamp", "context"]) {
  inspectorConsole[name] = () => {};
}

class PromiseSession extends Session {
  post(method, params) {
    return new Promise((resolve, reject) => {
      super.post(method, params, (error, result) => (error ? reject(error) : resolve(result)));
    });
  }
}

const Network = {};
for (const name of ["requestWillBeSent", "responseReceived", "loadingFinished", "loadingFailed", "dataReceived", "dataSent"]) {
  Network[name] = (params) => validate.object(params, "params");
}

function exportsWith(SessionClass) {
  return {
    open() {
      throw internal.unsupported("inspector.open (pi-rust has no DevTools server)");
    },
    close() {},
    url: () => undefined,
    waitForDebugger() {
      throw codeError(Error, "ERR_INSPECTOR_NOT_ACTIVE", "Inspector is not active");
    },
    console: inspectorConsole,
    Session: SessionClass,
    Network,
    NetworkResources: { put() {} },
  };
}

module.exports = exportsWith(Session);
Object.defineProperty(module.exports, "promises", { value: exportsWith(PromiseSession) });
