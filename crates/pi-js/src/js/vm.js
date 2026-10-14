// node:vm. Each context is a separate QuickJS context in this runtime. The contextified object's
// properties appear in it as accessor globals, and globals the code creates are copied back to
// the object after each run.
const { Buffer } = require("buffer");
const { invalidArgType, invalidArgValue, validate } = require("internal/errors");
const nativeVm = native.vm;

const contexts = new WeakMap();
const constants = Object.freeze({
  __proto__: null,
  USE_MAIN_CONTEXT_DEFAULT_LOADER: Symbol("vm_dynamic_import_main_context_default"),
  DONT_CONTEXTIFY: Symbol("vm_context_no_contextify"),
});

function validateObject(value, name) {
  if (value === null || typeof value !== "object") throw invalidArgType(name, "of type object", value);
}

function contextOptions(options, prefix) {
  if (options === undefined) return {};
  validate.object(options, prefix);
  const name = prefix === "options" ? "name" : "contextName";
  if (options[name] !== undefined) validate.string(options[name], `${prefix}.${name}`);
  const origin = prefix === "options" ? "origin" : "contextOrigin";
  if (options[origin] !== undefined) validate.string(options[origin], `${prefix}.${origin}`);
  const generation = prefix === "options" ? "codeGeneration" : "contextCodeGeneration";
  if (options[generation] !== undefined) validate.object(options[generation], `${prefix}.${generation}`);
  const mode = options.microtaskMode;
  if (mode !== undefined && mode !== "afterEvaluate") {
    throw invalidArgValue(`${prefix}.microtaskMode`, mode, "must be 'afterEvaluate' or undefined");
  }
  return { microtask: mode === "afterEvaluate" };
}

function isContext(object) {
  validateObject(object, "object");
  return contexts.has(object);
}

function createContext(contextObject = {}, options) {
  const { microtask } = contextOptions(options, "options");
  if (contextObject === constants.DONT_CONTEXTIFY) {
    const { id, global } = nativeVm.createContext();
    contexts.set(global, { id, global, sandbox: null, microtask });
    return global;
  }
  validateObject(contextObject, "contextObject");
  if (contexts.has(contextObject)) return contextObject;
  const { id, global } = nativeVm.createContext();
  const builtins = new Set(Reflect.ownKeys(global));
  contexts.set(contextObject, { id, global, sandbox: contextObject, builtins, forwarded: new Set(), copied: new Set(), microtask });
  return contextObject;
}

// Before a run: new properties of the object become accessor globals (or copies, when the
// global already has a fixed property of that name, as `var` declarations make).
function syncIn(entry) {
  const { global, sandbox, forwarded, copied } = entry;
  if (!sandbox) return;
  for (const key of Reflect.ownKeys(sandbox)) {
    if (forwarded.has(key)) continue;
    const own = Object.getOwnPropertyDescriptor(global, key);
    if (own && !own.configurable) {
      if (own.writable) global[key] = sandbox[key];
      copied.add(key);
      continue;
    }
    const enumerable = Object.getOwnPropertyDescriptor(sandbox, key).enumerable;
    Object.defineProperty(global, key, {
      get: () => sandbox[key],
      set: (value) => { sandbox[key] = value; },
      enumerable,
      configurable: true,
    });
    forwarded.add(key);
  }
}

function syncOut(entry) {
  const { global, sandbox, builtins, forwarded, copied } = entry;
  if (!sandbox) return;
  for (const key of Reflect.ownKeys(global)) {
    if (forwarded.has(key) || (builtins.has(key) && !copied.has(key))) continue;
    sandbox[key] = global[key];
    copied.add(key);
  }
}

function contextEntry(contextifiedObject) {
  const entry = contextifiedObject !== null && typeof contextifiedObject === "object" && contexts.get(contextifiedObject);
  if (!entry) throw invalidArgType("contextifiedObject", "an vm.Context", contextifiedObject);
  return entry;
}

function runOptions(options) {
  if (options === undefined) return {};
  if (typeof options === "string") return { filename: options };
  validate.object(options, "options");
  if (options.timeout !== undefined) validate.integer(options.timeout, "options.timeout", 1, 2 ** 32 - 1);
  if (options.displayErrors !== undefined) validate.boolean(options.displayErrors, "options.displayErrors");
  if (options.breakOnSigint !== undefined) validate.boolean(options.breakOnSigint, "options.breakOnSigint");
  return options;
}

function evaluate(entry, code, filename, options) {
  if (!entry) return nativeVm.run(0, code, filename, options.timeout, false);
  syncIn(entry);
  try {
    return nativeVm.run(entry.id, code, filename, options.timeout, entry.microtask);
  } finally {
    syncOut(entry);
  }
}

const kCode = Symbol("code");
const kFilename = Symbol("filename");

class Script {
  constructor(code, options = {}) {
    code = `${code}`;
    if (typeof options === "string") options = { filename: options };
    else validate.object(options, "options");
    const { filename = "evalmachine.<anonymous>", lineOffset = 0, columnOffset = 0, cachedData, produceCachedData = false,
      importModuleDynamically } = options;
    validate.string(filename, "options.filename");
    validate.integer(lineOffset, "options.lineOffset");
    validate.integer(columnOffset, "options.columnOffset");
    if (cachedData !== undefined && !ArrayBuffer.isView(cachedData)) {
      throw invalidArgType("options.cachedData", "an instance of Buffer, TypedArray, or DataView", cachedData);
    }
    validate.boolean(produceCachedData, "options.produceCachedData");
    if (importModuleDynamically !== undefined && typeof importModuleDynamically !== "function" &&
      !Object.values(constants).includes(importModuleDynamically)) {
      throw invalidArgType("options.importModuleDynamically", "of type function or a vm.constants symbol", importModuleDynamically);
    }
    this[kCode] = (lineOffset > 0 ? "\n".repeat(lineOffset) : "") + code;
    this[kFilename] = filename;
    nativeVm.check(this[kCode], filename);
    if (cachedData !== undefined) this.cachedDataRejected = false;
    if (produceCachedData) {
      this.cachedDataProduced = true;
      this.cachedData = this.createCachedData();
    }
    this.sourceMapURL = undefined;
  }
  runInThisContext(options) {
    return evaluate(null, this[kCode], this[kFilename], runOptions(options));
  }
  runInContext(contextifiedObject, options) {
    const entry = contextEntry(contextifiedObject);
    return evaluate(entry, this[kCode], this[kFilename], runOptions(options));
  }
  runInNewContext(contextObject, options) {
    const context = createContext(contextObject, newContextOptions(options));
    return this.runInContext(context, options);
  }
  createCachedData() {
    return Buffer.from(`pi-js-vm:${this[kCode].length}`);
  }
}

function newContextOptions(options) {
  if (options === undefined || typeof options !== "object" || options === null) return undefined;
  return { name: options.contextName, origin: options.contextOrigin, codeGeneration: options.contextCodeGeneration,
    microtaskMode: options.microtaskMode };
}

function runInThisContext(code, options) {
  return new Script(code, options).runInThisContext(options);
}

function runInContext(code, contextifiedObject, options) {
  contextEntry(contextifiedObject);
  return new Script(code, options).runInContext(contextifiedObject, options);
}

function runInNewContext(code, contextObject, options) {
  return new Script(code, options).runInNewContext(contextObject, options);
}

function compileFunction(code, params = [], options = {}) {
  validate.string(code, "code");
  validate.array(params, "params");
  params.forEach((param, index) => validate.string(param, `params[${index}]`));
  validate.object(options, "options");
  const { filename = "", lineOffset = 0, columnOffset = 0, cachedData, produceCachedData = false, parsingContext,
    contextExtensions = [] } = options;
  validate.string(filename, "options.filename");
  validate.integer(lineOffset, "options.lineOffset");
  validate.integer(columnOffset, "options.columnOffset");
  if (cachedData !== undefined && !ArrayBuffer.isView(cachedData)) {
    throw invalidArgType("options.cachedData", "an instance of Buffer, TypedArray, or DataView", cachedData);
  }
  validate.boolean(produceCachedData, "options.produceCachedData");
  let entry = null;
  if (parsingContext !== undefined) {
    if (parsingContext === null || typeof parsingContext !== "object" || !contexts.has(parsingContext)) {
      throw invalidArgType("options.parsingContext", "a Context", parsingContext);
    }
    entry = contexts.get(parsingContext);
  }
  validate.array(contextExtensions, "options.contextExtensions");
  contextExtensions.forEach((extension, index) => validateObject(extension, `options.contextExtensions[${index}]`));
  const head = lineOffset > 0 ? "\n".repeat(lineOffset) : "";
  const inner = `function (${params.join(", ")}) {\n${code}\n}`;
  let source = `${head}(${inner})`;
  if (contextExtensions.length) {
    const opened = contextExtensions.map((_, index) => `with (__pi_extensions[${index}]) {`).join(" ");
    source = `${head}(function (__pi_extensions) { ${opened} return ${inner}; ${"}".repeat(contextExtensions.length)} })`;
  }
  const result = evaluate(entry, source, filename, {});
  const fn = contextExtensions.length ? result(contextExtensions) : result;
  if (produceCachedData) {
    fn.cachedDataProduced = true;
    fn.cachedData = Buffer.from(`pi-js-vm:${code.length}`);
  }
  if (cachedData !== undefined) fn.cachedDataRejected = false;
  return fn;
}

function measureMemory(options = {}) {
  validate.object(options, "options");
  const { mode = "summary", execution = "default" } = options;
  if (mode !== "summary" && mode !== "detailed") throw invalidArgValue("options.mode", mode, "must be 'summary' or 'detailed'");
  if (execution !== "default" && execution !== "eager") {
    throw invalidArgValue("options.execution", execution, "must be 'default' or 'eager'");
  }
  const used = process.memoryUsage().heapUsed;
  const result = { total: { jsMemoryEstimate: used, jsMemoryRange: [used, used] } };
  if (mode === "detailed") result.current = result.total;
  return Promise.resolve(result);
}

module.exports = {
  Script,
  createContext,
  createScript: (code, options) => new Script(code, options),
  runInContext,
  runInNewContext,
  runInThisContext,
  isContext,
  compileFunction,
  measureMemory,
  constants,
};
