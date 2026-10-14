// The WebAssembly global on the wasmi interpreter: compiling, instantiating with imports
// resolved as V8 resolves them, and the namespace. Values, memories, tables, and globals are in
// wasm_values.js.
const { codeError, describe } = require("internal/errors");
const values = require("internal/wasm_values");
const { ids, funcNames, CompileError, LinkError, RuntimeError, Memory, Table, Global } = values;
const { defineInterface, guard, idOf, wrap, toWasm, functionFor } = values;
const wasm = native.wasm;

function bytesOf(source, prefix, copy, message = "Argument 0 must be a buffer source") {
  let bytes;
  if (source instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && source instanceof SharedArrayBuffer)) {
    bytes = new Uint8Array(source);
  } else if (ArrayBuffer.isView(source)) {
    bytes = new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  } else {
    throw new TypeError(`${prefix}: ${message}`);
  }
  return copy ? bytes.slice() : bytes;
}

// wasmparser's messages for the common header errors in V8's words, and offsets as "@+N".
function compileMessage(message, bytes) {
  const hex = (from) => [...bytes.subarray(from, from + 4)].map((byte) => byte.toString(16).padStart(2, "0")).join(" ");
  if (message.startsWith("magic header not detected") && bytes.length >= 4) {
    return `expected magic word 00 61 73 6d, found ${hex(0)} @+0`;
  }
  if (message.startsWith("unknown binary version") && bytes.length >= 8) {
    return `expected version 01 00 00 00, found ${hex(4)} @+4`;
  }
  return message.replace(/ \(at offset 0x([0-9a-f]+)\)$/, (_, offset) => ` @+${parseInt(offset, 16)}`);
}

function compileBytes(bytes, prefix) {
  if (bytes.byteLength === 0) throw new CompileError(`${prefix}: BufferSource argument is empty`);
  try {
    return wrap(Module, wasm.compile(bytes));
  } catch (error) {
    if (error?.code !== "ERR_PI_WASM") throw error;
    throw new CompileError(`${prefix}: ${compileMessage(error.message, bytes)}`);
  }
}

function importFailure(Kind, prefix, index, module, name, message) {
  const field = name === undefined ? "" : ` function="${name}"`;
  return new Kind(`${prefix}: Import #${index} module="${module}"${field}: ${message}`);
}

function resolveGlobal(value, descriptor, fail) {
  if (value instanceof Global) {
    const info = wasm.globalInfo(ids.get(value));
    if (info.value !== descriptor.type) throw fail(LinkError, "imported global does not match the expected type");
    if (info.mutable !== descriptor.mutable) throw fail(LinkError, "imported global does not match the expected mutability");
    return { kind: "id", id: ids.get(value) };
  }
  const valid =
    descriptor.type === "externref" ||
    (descriptor.type === "funcref" && (value === null || ids.has(value))) ||
    (descriptor.type === "i64" ? typeof value === "bigint" : typeof value === "number");
  if (!valid) throw fail(LinkError, "global import must be a number, valid Wasm reference, or WebAssembly.Global object");
  if (descriptor.mutable) throw fail(LinkError, "imported mutable global must be a WebAssembly.Global object");
  return { kind: "value", value: toWasm(value, descriptor.type) };
}

function resolveImport(value, descriptor, fail) {
  switch (descriptor.kind) {
    case "function":
      if (typeof value !== "function") throw fail(LinkError, "function import requires a callable");
      return ids.has(value) ? { kind: "id", id: ids.get(value) } : { kind: "js", fn: value };
    case "memory":
      if (!(value instanceof Memory)) throw fail(LinkError, "memory import must be a WebAssembly.Memory object");
      return { kind: "id", id: ids.get(value) };
    case "table":
      if (!(value instanceof Table)) throw fail(LinkError, "table import requires a WebAssembly.Table");
      return { kind: "id", id: ids.get(value) };
    default:
      return resolveGlobal(value, descriptor, fail);
  }
}

// Each import as the native layer takes it, with the "Import #i" prefix for its link errors.
function resolveImports(moduleId, importObject, prefix) {
  const descriptors = wasm.moduleImports(moduleId);
  const missing = importObject === undefined ? descriptors.length > 0 : importObject === null || typeof importObject !== "object";
  if (missing) throw new TypeError(`${prefix}: Imports argument must be present and must be an object`);
  return descriptors.map((descriptor, index) => {
    const namespace = importObject[descriptor.module];
    if (namespace === null || (typeof namespace !== "object" && typeof namespace !== "function")) {
      throw importFailure(TypeError, prefix, index, descriptor.module, undefined, "module is not an object or function");
    }
    const fail = (Kind, message) => importFailure(Kind, prefix, index, descriptor.module, descriptor.name, message);
    const resolved = resolveImport(namespace[descriptor.name], descriptor, fail);
    return { ...resolved, prefix: fail(Error, "").message.slice(prefix.length + 2) };
  });
}

function instantiateModule(module, importObject, prefix) {
  const moduleId = idOf(module, Module, prefix, "Argument 0 must be a WebAssembly.Module");
  const imports = resolveImports(moduleId, importObject, prefix);
  const result = guard(`${prefix}: `, () => wasm.instantiate(moduleId, imports));
  for (const [id, index] of result.functions) if (!funcNames.has(id)) funcNames.set(id, index);
  const exports = Object.create(null);
  for (const { name, kind, id } of result.exports) {
    exports[name] = kind === "function" ? functionFor(id) : wrap({ memory: Memory, table: Table, global: Global }[kind], id);
  }
  Object.freeze(exports);
  if (result.start !== undefined) functionFor(result.start)();
  const instance = Object.create(Instance.prototype);
  ids.set(instance, exports);
  return instance;
}

function moduleId(module, method) {
  return idOf(module, Module, `WebAssembly.Module.${method}()`, "Argument 0 must be a WebAssembly.Module");
}

const Module = defineInterface(
  "Module",
  (bytes) => compileBytes(bytesOf(bytes, "WebAssembly.Module()", false), "WebAssembly.Module()"),
  {},
  {
    imports(module) {
      return wasm.moduleImports(moduleId(module, "imports")).map(({ module, name, kind }) => ({ module, name, kind }));
    },
    exports(module) {
      return wasm.moduleExports(moduleId(module, "exports"));
    },
    customSections(module, sectionName) {
      const id = moduleId(module, "customSections");
      if (sectionName === undefined) throw new TypeError("WebAssembly.Module.customSections(): Argument 1 is required");
      return wasm.customSections(id, String(sectionName));
    },
  },
);

const Instance = defineInterface(
  "Instance",
  (module, importObject) => instantiateModule(module, importObject, "WebAssembly.Instance()"),
  {
    get exports() {
      return idOf(this, Instance, "WebAssembly.Instance.exports", "Receiver is not a WebAssembly.Instance");
    },
  },
);

function unsupportedClass(name) {
  return function () {
    throw internal.unsupported(`WebAssembly.${name} (exception handling)`);
  };
}

function settle(fn) {
  return new Promise((resolve) => resolve(fn()));
}

function instantiate(source, importObject) {
  const prefix = "WebAssembly.instantiate()";
  if (source instanceof Module) return settle(() => instantiateModule(source, importObject, prefix));
  let bytes;
  try {
    bytes = bytesOf(source, prefix, true, "Argument 0 must be a buffer source or a WebAssembly.Module object");
  } catch (error) {
    return Promise.reject(error);
  }
  return settle(() => {
    const module = compileBytes(bytes, prefix);
    return { module, instance: instantiateModule(module, importObject, prefix) };
  });
}

// Node's streaming callback: the source must resolve to an ok application/wasm Response.
async function responseModule(source, prefix) {
  const response = await source;
  const { Response } = require("internal/fetch");
  if (!(response instanceof Response)) {
    const expected = "an instance of Response or an Promise resolving to Response";
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "source" argument must be ${expected}. Received ${describe(response)}`);
  }
  const fail = (message) => codeError(TypeError, "ERR_WEBASSEMBLY_RESPONSE", `WebAssembly response ${message}`);
  const type = response.headers.get("Content-Type");
  if (type !== "application/wasm") throw fail(`has unsupported MIME type '${type}'`);
  if (!response.ok) throw fail(`has status code ${response.status}`);
  if (response.bodyUsed) throw fail("body has already been used");
  return compileBytes(new Uint8Array(await response.arrayBuffer()), prefix);
}

const WebAssembly = {
  compile(bytes) {
    try {
      bytes = bytesOf(bytes, "WebAssembly.compile()", true);
    } catch (error) {
      return Promise.reject(error);
    }
    return settle(() => compileBytes(bytes, "WebAssembly.compile()"));
  },
  validate(bytes) {
    const view = bytesOf(bytes, "WebAssembly.validate()", false);
    return view.byteLength > 0 && wasm.validate(view);
  },
  instantiate,
  compileStreaming(source) {
    return responseModule(source, "WebAssembly.compileStreaming()");
  },
  instantiateStreaming(source, importObject) {
    const prefix = "WebAssembly.instantiateStreaming()";
    return responseModule(source, prefix).then((module) => ({ module, instance: instantiateModule(module, importObject, prefix) }));
  },
};
const classes = { Module, Instance, Table, Memory, Global, Tag: unsupportedClass("Tag"), Exception: unsupportedClass("Exception") };
for (const [name, value] of Object.entries({ ...classes, CompileError, LinkError, RuntimeError })) {
  Object.defineProperty(WebAssembly, name, { value, writable: true, configurable: true });
}
Object.defineProperty(WebAssembly, Symbol.toStringTag, { value: "WebAssembly", configurable: true });

module.exports = WebAssembly;
