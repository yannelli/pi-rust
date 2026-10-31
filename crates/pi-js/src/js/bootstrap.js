// Bootstrap: the CommonJS loader, ES module facades, and lazily loaded Node and web globals.
(function bootstrap(native, config) {
  "use strict";
  const internal = Object.create(null);
  internal.config = config;
  Object.defineProperty(globalThis, "__pi_internal", { value: internal, enumerable: false, configurable: false, writable: false });

  // Async context. The frame lives in Rust, which resets it after each job and callback;
  // promise reactions, ticks, timers, and instrumented awaits restore the frame they captured.
  const asyncContext = native.asyncContext;
  const promiseThen = Promise.prototype.then;
  let contextUsed = false;
  internal.asyncContext = asyncContext;
  internal.promiseThen = promiseThen;
  internal.enableAsyncContext = () => {
    contextUsed = true;
  };
  const thrownFrames = new WeakMap();
  internal.thrownFrame = (error) => thrownFrames.get(error);
  function runInFrame(frame, fn, self, args) {
    const previous = asyncContext.get();
    asyncContext.set(frame);
    try {
      return Reflect.apply(fn, self, args);
    } catch (error) {
      if (error !== null && typeof error === "object" && !thrownFrames.has(error)) thrownFrames.set(error, frame);
      throw error;
    } finally {
      asyncContext.set(previous);
    }
  }
  internal.runInFrame = runInFrame;
  function bindFrame(fn, frame) {
    return function bound(...args) {
      return runInFrame(frame, fn, this, args);
    };
  }
  internal.bindContext = function bindContext(fn) {
    if (!contextUsed || typeof fn !== "function") return fn;
    const frame = asyncContext.get();
    return frame === undefined ? fn : bindFrame(fn, frame);
  };
  Object.defineProperty(Promise.prototype, "then", {
    value: function then(onFulfilled, onRejected) {
      const frame = contextUsed ? asyncContext.get() : undefined;
      if (frame === undefined) return promiseThen.call(this, onFulfilled, onRejected);
      const fulfilled = typeof onFulfilled === "function" ? bindFrame(onFulfilled, frame) : onFulfilled;
      const rejected = typeof onRejected === "function" ? bindFrame(onRejected, frame) : onRejected;
      return promiseThen.call(this, fulfilled, rejected);
    },
    writable: true,
    configurable: true,
  });
  class AwaitResult {
    constructor(frame, ok, value) {
      this.frame = frame;
      this.ok = ok;
      this.value = value;
    }
  }
  function awaitWrap(value) {
    const frame = contextUsed ? asyncContext.get() : undefined;
    if (frame === undefined) return value;
    if (value === null || (typeof value !== "object" && typeof value !== "function") || typeof value.then !== "function") {
      return new AwaitResult(frame, true, value);
    }
    return promiseThen.call(Promise.resolve(value), (v) => new AwaitResult(frame, true, v), (e) => new AwaitResult(frame, false, e));
  }
  function awaitUnwrap(result) {
    if (!(result instanceof AwaitResult)) return result;
    asyncContext.set(result.frame);
    if (!result.ok) throw result.value;
    return result.value;
  }
  Object.defineProperty(globalThis, "__pi_aw", { value: awaitWrap });
  Object.defineProperty(globalThis, "__pi_au", { value: awaitUnwrap });
  if (typeof globalThis.queueMicrotask === "function") {
    const queue = globalThis.queueMicrotask;
    Object.defineProperty(globalThis, "queueMicrotask", {
      value: function queueMicrotask(callback) {
        return queue(internal.bindContext(callback));
      },
      writable: true,
      configurable: true,
    });
  }

  const builtinCache = new Map();
  function loadBuiltin(name) {
    if (name.startsWith("node:")) name = name.slice(5);
    const cached = builtinCache.get(name);
    if (cached) return cached.exports;
    const factory = native.builtins.compile(name);
    const module = { exports: {}, id: name.includes(":") ? name : "node:" + name };
    builtinCache.set(name, module);
    try {
      factory.call(module.exports, module, module.exports, loadBuiltin, native, internal);
    } catch (error) {
      builtinCache.delete(name);
      throw error;
    }
    return module.exports;
  }
  internal.builtin = loadBuiltin;

  internal.bytesToLatin1 = function bytesToLatin1(value) {
    let view;
    if (value instanceof Uint8Array) view = value;
    else if (ArrayBuffer.isView(value)) view = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    else if (value instanceof ArrayBuffer) view = new Uint8Array(value);
    else if (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer) view = new Uint8Array(value);
    else return undefined;
    let out = "";
    for (let i = 0; i < view.length; i += 8192) out += String.fromCharCode.apply(null, view.subarray(i, i + 8192));
    return out;
  };

  internal.unsupported = function unsupported(api) {
    const error = new Error(api + " is not supported in pi-rust");
    error.code = "ERR_PI_UNSUPPORTED";
    return error;
  };

  // CommonJS modules.
  const path = loadBuiltin("path");
  const moduleCache = Object.create(null);
  let mainModule;

  function nodeModulePaths(from) {
    const paths = [];
    let dir = path.resolve(from);
    for (;;) {
      if (path.basename(dir) !== "node_modules") paths.push(path.join(dir, "node_modules"));
      const parent = path.dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
    return paths;
  }

  class Module {
    constructor(id = "", parent = null) {
      this.id = id;
      this.path = path.dirname(id);
      this.exports = {};
      this.filename = null;
      this.loaded = false;
      this.children = [];
      this.paths = nodeModulePaths(this.path);
      this.parent = parent;
      if (parent) parent.children.push(this);
    }
    require(id) {
      return requireFrom(id, this);
    }
  }

  function makeRequire(module) {
    const require = function require(id) {
      if (typeof id !== "string" || id === "") throw new TypeError("The \"id\" argument must be a non-empty string");
      return requireFrom(id, module);
    };
    require.resolve = function resolve(request) {
      const resolved = native.modules.resolve(request, module.filename, "require");
      return resolved.kind === "file" ? resolved.key : request;
    };
    require.resolve.paths = (request) => (isBuiltin(request) ? null : module.paths);
    require.main = mainModule;
    require.cache = moduleCache;
    require.extensions = Module._extensions;
    return require;
  }

  function requireFrom(id, parent) {
    const resolved = resolveRequest(id, parent ? parent.filename : null);
    if (resolved.kind === "builtin") return loadBuiltin(resolved.key);
    if (resolved.kind === "virtual") return virtualValue(resolved.key);
    return requireFile(resolved.key, parent);
  }

  // Extensions added to require.extensions also resolve extensionless relative requests.
  function resolveRequest(id, parentFile) {
    try {
      return native.modules.resolve(id, parentFile, "require");
    } catch (error) {
      if (!/^\.{0,2}[\\/]|^[A-Za-z]:[\\/]/.test(id) && !id.startsWith(".")) throw error;
      for (const ext of Object.keys(Module._extensions)) {
        if (Module._extensions[ext][kBuiltinHandler]) continue;
        try {
          return native.modules.resolve(id + ext, parentFile, "require");
        } catch {}
      }
      throw error;
    }
  }

  function findExtension(filename) {
    const name = path.basename(filename);
    for (let index = name.indexOf(".", 1); index !== -1; index = name.indexOf(".", index + 1)) {
      if (Module._extensions[name.slice(index)]) return name.slice(index);
    }
    return ".js";
  }

  function requireFile(filename, parent) {
    const cached = moduleCache[filename];
    if (cached) {
      if (parent && !parent.children.includes(cached)) parent.children.push(cached);
      return cached.exports;
    }
    const handler = Module._extensions[findExtension(filename)];
    if (typeof handler === "function" && !handler[kBuiltinHandler]) return requireWith(handler, filename, parent);
    const compiled = native.modules.compile(filename);
    if (compiled.format === "esm") return esmForRequire(native.modules.importSync(filename));
    const module = new Module(filename, parent);
    module.filename = filename;
    if (!mainModule && !parent) mainModule = module;
    moduleCache[filename] = module;
    let ok = false;
    try {
      if (compiled.format === "json") {
        try {
          module.exports = JSON.parse(compiled.text.charCodeAt(0) === 0xfeff ? compiled.text.slice(1) : compiled.text);
        } catch (error) {
          error.message = filename + ": " + error.message;
          throw error;
        }
      } else {
        compiled.fn.call(module.exports, module.exports, makeRequire(module), module, filename, module.path);
      }
      ok = true;
    } finally {
      if (!ok) delete moduleCache[filename];
    }
    module.loaded = true;
    return module.exports;
  }

  function requireWith(handler, filename, parent) {
    const module = new Module(filename, parent);
    module.filename = filename;
    if (!mainModule && !parent) mainModule = module;
    moduleCache[filename] = module;
    let ok = false;
    try {
      handler(module, filename);
      ok = true;
    } finally {
      if (!ok) delete moduleCache[filename];
    }
    module.loaded = true;
    return module.exports;
  }

  // The built-in handlers, also callable from user handlers that wrap them.
  const kBuiltinHandler = Symbol("builtinHandler");
  function builtinHandler(module, filename) {
    const compiled = native.modules.compile(filename);
    if (compiled.format === "esm") module.exports = esmForRequire(native.modules.importSync(filename));
    else if (compiled.format === "json") module.exports = JSON.parse(compiled.text.charCodeAt(0) === 0xfeff ? compiled.text.slice(1) : compiled.text);
    else compiled.fn.call(module.exports, module.exports, makeRequire(module), module, filename, module.path);
  }
  builtinHandler[kBuiltinHandler] = true;
  Module.prototype._compile = function _compile(content, filename) {
    const compiled = native.modules.compile(filename, String(content));
    if (compiled.format !== "cjs") {
      throw Object.assign(new Error(`require() of ES Module ${filename} not supported.`), { code: "ERR_REQUIRE_ESM" });
    }
    return compiled.fn.call(this.exports, this.exports, makeRequire(this), this, filename, path.dirname(filename));
  };

  // require() of an ES module returns its namespace, marked for Babel-style default interop.
  const esmWrappers = new WeakMap();
  function esmForRequire(namespace) {
    if (!("default" in namespace)) return namespace;
    let wrapped = esmWrappers.get(namespace);
    if (!wrapped) {
      wrapped = {};
      for (const key of Object.keys(namespace)) {
        Object.defineProperty(wrapped, key, { enumerable: true, get: () => namespace[key] });
      }
      Object.defineProperty(wrapped, "__esModule", { value: true });
      esmWrappers.set(namespace, wrapped);
    }
    return wrapped;
  }

  const virtualValues = new Map();
  function virtualValue(key) {
    if (virtualValues.has(key)) return virtualValues.get(key);
    const result = native.modules.virtualValue(key);
    const value = result.source ? esmForRequire(native.modules.importSync(key)) : result.value;
    virtualValues.set(key, value);
    return value;
  }

  function isBuiltin(request) {
    if (request.startsWith("node:")) return builtinNames(true).includes(request.slice(5));
    return builtinNames().includes(request);
  }
  let builtinList;
  let prefixedList;
  // Modules such as node:sqlite and node:test load only with the prefix, so they are listed
  // only for prefixed names.
  function builtinNames(prefixed) {
    if (!builtinList) {
      const prefixOnly = native.builtins.prefixOnly();
      prefixedList = native.builtins.list();
      builtinList = prefixedList.filter((name) => !prefixOnly.includes(name));
    }
    return prefixed ? prefixedList : builtinList;
  }

  function createRequire(filename) {
    if (filename && typeof filename === "object" && typeof filename.href === "string") filename = filename.href;
    if (typeof filename !== "string") throw new TypeError("createRequire expects a path or file URL");
    if (filename.startsWith("file:")) filename = loadBuiltin("url").fileURLToPath(filename);
    const module = new Module(filename, null);
    module.filename = filename;
    module.loaded = true;
    return makeRequire(module);
  }

  Module._cache = moduleCache;
  Module._extensions = Object.create(null);
  for (const ext of [".js", ".json", ".cjs", ".mjs", ".ts", ".tsx", ".mts", ".cts", ".jsx"]) {
    Module._extensions[ext] = builtinHandler;
  }
  Module._extensions[".node"] = function loadAddon() {
    throw internal.unsupported("process.dlopen");
  };
  Module.createRequire = createRequire;
  Module.isBuiltin = isBuiltin;
  Module.Module = Module;
  Module.wrap = (script) => "(function (exports, require, module, __filename, __dirname) { " + script + "\n});";
  Module._nodeModulePaths = nodeModulePaths;
  Module._resolveFilename = (request, parent) => makeRequire(parent || new Module(config.cwd + "/[eval]")).resolve(request);
  Module._load = (request, parent) => requireFrom(request, parent || null);
  Object.defineProperty(Module, "builtinModules", { enumerable: true, get: () => builtinNames().slice() });
  internal.Module = Module;
  internal.createRequire = createRequire;
  internal.requireFile = requireFile;

  internal.resolveUrl = function resolveUrl(specifier, parentPath) {
    if (internal.hookResolve && native.modules.hooksEnabled()) {
      const key = internal.hookResolve(String(specifier), parentPath);
      return path.isAbsolute(key) ? loadBuiltin("url").pathToFileURL(key).href : key;
    }
    const resolved = native.modules.resolve(String(specifier), parentPath, "import");
    return resolved.kind === "file" ? loadBuiltin("url").pathToFileURL(resolved.key).href : resolved.key;
  };

  // Facades re-export an object's keys so ES modules can import CommonJS, JSON, and values.
  const facades = new Map();
  internal.facade = (key) => facades.get(key);
  const IDENTIFIER = /^[A-Za-z_$][\w$]*$/;
  const RESERVED = new Set(("break case catch class const continue debugger default delete do else enum export extends " +
    "false finally for function if import in instanceof new null return super switch this throw true try typeof var " +
    "void while with yield let static implements interface package private protected public await arguments eval __m").split(" "));
  function facadeSource(key, value, interop) {
    facades.set(key, value);
    let source = "const __m = globalThis.__pi_internal.facade(" + JSON.stringify(key) + ");\n";
    const isObject = value !== null && (typeof value === "object" || typeof value === "function");
    source += "export default " + (interop && isObject && value.__esModule ? "__m.default" : "__m") + ";\n";
    if (isObject) {
      const names = Object.keys(value).filter((name) => IDENTIFIER.test(name) && !RESERVED.has(name) && name !== "__esModule");
      if (names.length) source += "export const { " + names.join(", ") + " } = __m;\n";
    }
    return source;
  }
  internal.cjsFacade = (filename, source) => {
    const compileSource = (module, file) => module._compile(source, file);
    const exports = source === undefined || moduleCache[filename] ? requireFile(filename, null) : requireWith(compileSource, filename, null);
    return facadeSource(filename, exports, true);
  };
  internal.builtinFacade = (name) => facadeSource(name.includes(":") ? name : "node:" + name, loadBuiltin(name), false);
  internal.virtualFacade = (key) => facadeSource(key, virtualValue(key), false);
  internal.valueFacade = (key, value) => facadeSource(key, value, false);

  // Uncaught errors go to process listeners when there are any.
  internal.handleUncaught = function handleUncaught(kind, error) {
    if (kind === "uncaughtException" && internal.domainHandler && internal.domainHandler(error)) return true;
    const process = builtinCache.get("process");
    if (!process || process.exports.listenerCount(kind) === 0) return internal.workerUncaught ? internal.workerUncaught(error) : false;
    process.exports.emit(kind, error);
    return true;
  };

  // Globals, loaded on first use.
  function defineLazy(name, load) {
    Object.defineProperty(globalThis, name, {
      configurable: true,
      enumerable: false,
      get() {
        const value = load();
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: false });
        return value;
      },
      set(value) {
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: false });
      },
    });
  }
  internal.defineLazy = defineLazy;
  // A WebAssembly.Memory buffer views the engine's memory, which moves when it grows, so like V8 it
  // cannot be detached.
  const nonDetachable = new WeakSet();
  internal.nonDetachable = nonDetachable;
  for (const name of ["transfer", "transferToFixedLength", "transferToImmutable"]) {
    const descriptor = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, name);
    if (!descriptor) continue;
    const original = descriptor.value;
    const method = {
      [name](...args) {
        if (nonDetachable.has(this)) throw new TypeError("ArrayBuffer is not detachable and could not be cloned.");
        return Reflect.apply(original, this, args);
      },
    }[name];
    Object.defineProperty(method, "length", { value: original.length });
    Object.defineProperty(ArrayBuffer.prototype, name, { ...descriptor, value: method });
  }
  Object.defineProperty(globalThis, "global", { value: globalThis, writable: true, configurable: true });
  defineLazy("process", () => loadBuiltin("process"));
  defineLazy("console", () => loadBuiltin("console"));
  for (const name of ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "setImmediate", "clearImmediate"]) {
    defineLazy(name, () => loadBuiltin("timers")[name]);
  }
  const lazyGlobals = {
    buffer: ["Buffer", "Blob", "atob", "btoa"],
    url: ["URL", "URLSearchParams"],
    "internal/web": ["EventTarget", "Event", "CustomEvent", "AbortController", "AbortSignal", "DOMException",
      "TextEncoder", "TextDecoder"],
    "internal/worker_ports": ["MessageChannel", "MessagePort", "BroadcastChannel", "MessageEvent", "structuredClone"],
    "internal/fetch": ["fetch", "Headers", "Request", "Response", "FormData"],
    "stream/web": ["ReadableStream", "ReadableStreamDefaultReader", "ReadableStreamDefaultController", "WritableStream",
      "WritableStreamDefaultWriter", "WritableStreamDefaultController", "TransformStream", "TransformStreamDefaultController",
      "ByteLengthQueuingStrategy", "CountQueuingStrategy", "TextEncoderStream", "TextDecoderStream", "CompressionStream",
      "DecompressionStream", "ReadableStreamBYOBReader", "ReadableByteStreamController", "ReadableStreamBYOBRequest"],
    perf_hooks: ["performance", "PerformanceObserver"],
  };
  for (const [module, names] of Object.entries(lazyGlobals)) {
    for (const name of names) defineLazy(name, () => loadBuiltin(module)[name]);
  }
  defineLazy("crypto", () => loadBuiltin("crypto").webcrypto);
  defineLazy("WebAssembly", () => loadBuiltin("internal/webassembly"));
  if (typeof globalThis.Intl === "undefined") {
    defineLazy("Intl", () => loadBuiltin("internal/intl"));
    // Node formats numbers for `toLocaleString` with Intl.NumberFormat; QuickJS adds no grouping.
    const numberValueOf = Number.prototype.valueOf;
    const methods = {
      toLocaleString() {
        let value;
        try {
          value = numberValueOf.call(this);
        } catch {
          throw new TypeError("Number.prototype.toLocaleString requires that 'this' be a Number");
        }
        return new Intl.NumberFormat(arguments[0], arguments[1]).format(value);
      },
    };
    Object.defineProperty(Number.prototype, "toLocaleString", {
      value: methods.toLocaleString,
      writable: true,
      configurable: true,
    });
  }
  defineLazy("navigator", () => ({
    hardwareConcurrency: loadBuiltin("os").availableParallelism(),
    language: "en-US",
    platform: config.platform,
    userAgent: "Node.js/" + config.nodeVersion.split(".")[0],
  }));

  // Helpers called by transpiled TypeScript decorators (oxc external helper mode).
  const babelHelpers = {
    decorate(decorators, target, key, desc) {
      const count = arguments.length;
      let result = count < 3 ? target : desc === null ? (desc = Object.getOwnPropertyDescriptor(target, key)) : desc;
      for (let i = decorators.length - 1; i >= 0; i--) {
        const decorator = decorators[i];
        if (decorator) result = (count < 3 ? decorator(result) : count > 3 ? decorator(target, key, result) : decorator(target, key)) || result;
      }
      if (count > 3 && result) Object.defineProperty(target, key, result);
      return result;
    },
    decorateParam(index, decorator) {
      return (target, key) => decorator(target, key, index);
    },
    decorateMetadata(key, value) {
      if (typeof Reflect === "object" && typeof Reflect.metadata === "function") return Reflect.metadata(key, value);
      return undefined;
    },
  };
  Object.defineProperty(globalThis, "babelHelpers", { value: babelHelpers, writable: true, configurable: true });

  return internal;
});
