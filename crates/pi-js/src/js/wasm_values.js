// Values and objects for the WebAssembly global: entities live in the native wasmi store and are
// held here by id. This file coerces values as V8 does and defines Memory, Table, and Global;
// webassembly.js adds modules, instances, and the namespace.
const wasm = native.wasm;

const ids = new WeakMap();
const wrappers = new Map();
const funcNames = new Map();
const externIds = new Map();
const externValues = [];
const VALUE_TYPES = { i32: "i32", i64: "i64", f32: "f32", f64: "f64", anyfunc: "funcref", funcref: "funcref", externref: "externref", v128: "v128" };
const REFERENCE_TYPES = { anyfunc: "funcref", funcref: "funcref", externref: "externref" };
const DEFAULTS = { i32: 0, i64: 0n, f32: 0, f64: 0, funcref: null, externref: undefined };

function errorClass(name) {
  const Class = { [name]: class extends Error {} }[name];
  Object.defineProperty(Class.prototype, "name", { value: name, writable: true, configurable: true });
  return Class;
}
const CompileError = errorClass("CompileError");
const LinkError = errorClass("LinkError");
const RuntimeError = errorClass("RuntimeError");
const KINDS = { CompileError, LinkError, RuntimeError, RangeError, TypeError };

// Turn a native error into its WebAssembly error class; other errors pass through.
function convert(error, prefix) {
  if (error === null || typeof error !== "object" || error.code !== "ERR_PI_WASM") return error;
  return new (KINDS[error.wasmKind] || RuntimeError)(prefix + error.message);
}

function guard(prefix, fn) {
  try {
    return fn();
  } catch (error) {
    throw convert(error, prefix);
  }
}

function idOf(value, Class, prefix, message) {
  const id = value !== null && typeof value === "object" && value instanceof Class ? ids.get(value) : undefined;
  if (id === undefined) throw new TypeError(`${prefix}: ${message}`);
  return id;
}

// The one JavaScript object for entity `id`, so an export is identical to what was imported.
function wrap(Class, id, object) {
  const key = `${Class.name}:${id}`;
  if (!object && wrappers.has(key)) return wrappers.get(key);
  object ??= Object.create(Class.prototype);
  ids.set(object, id);
  wrappers.set(key, object);
  return object;
}

// A WebIDL-style interface as V8 defines it: a constructor that requires `new`, enumerable
// members, and a "WebAssembly.X" tag.
function defineInterface(name, init, members, statics = {}) {
  const Class = {
    [name]: function (...args) {
      if (new.target === undefined) throw new TypeError(`WebAssembly.${name}(): WebAssembly.${name} must be invoked with 'new'`);
      return init.apply(this, args);
    },
  }[name];
  Object.defineProperty(Class, "length", { value: 1 });
  Object.defineProperties(Class, Object.getOwnPropertyDescriptors(statics));
  Object.defineProperties(Class.prototype, Object.getOwnPropertyDescriptors(members));
  Object.defineProperty(Class.prototype, Symbol.toStringTag, { value: `WebAssembly.${name}`, configurable: true });
  return Class;
}

function toPrimitive(value) {
  if ((value === null || typeof value !== "object") && typeof value !== "function") return value;
  if (typeof value[Symbol.toPrimitive] === "function") return value[Symbol.toPrimitive]("number");
  const primitive = typeof value.valueOf === "function" ? value.valueOf() : value;
  return primitive !== null && typeof primitive === "object" ? String(value) : primitive;
}

function toBigInt64(value) {
  value = toPrimitive(value);
  switch (typeof value) {
    case "bigint":
      return BigInt.asIntN(64, value);
    case "boolean":
      return value ? 1n : 0n;
    case "string":
      try {
        return BigInt.asIntN(64, BigInt(value));
      } catch {
        throw new SyntaxError(`Cannot convert ${value} to a BigInt`);
      }
    default:
      throw new TypeError(`Cannot convert ${String(value)} to a BigInt`);
  }
}

function incompatible() {
  return new TypeError("type incompatibility when transforming from/to JS");
}

// A JavaScript value as the native layer takes it for a Wasm value of `type`.
function toWasm(value, type) {
  if (typeof value === "bigint" && type !== "i64" && type !== "externref") {
    throw new TypeError("Cannot convert a BigInt value to a number");
  }
  switch (type) {
    case "i32":
      return value | 0;
    case "i64":
      return toBigInt64(value);
    case "f32":
    case "f64":
      return +value;
    case "externref":
      if (value === null) return null;
      if (!externIds.has(value)) externIds.set(value, externValues.push(value) - 1);
      return externIds.get(value);
    case "funcref":
      if (value === null) return null;
      if (typeof value === "function" && ids.has(value)) return ids.get(value);
      throw incompatible();
    default:
      throw incompatible();
  }
}

function fromWasm(value, type) {
  if (type === "externref") return value === null ? null : externValues[value];
  if (type === "funcref") return value === null ? null : functionFor(value);
  return value;
}

// The exported function for function `id`, named by its index as V8 names it.
function functionFor(id) {
  let fn = wrappers.get(`function:${id}`);
  if (fn) return fn;
  const { params, results } = wasm.funcType(id);
  fn = (...args) => {
    const values = params.map((type, index) => toWasm(args[index], type));
    const out = guard("", () => wasm.call(id, values));
    if (results.length < 2) return results.length ? fromWasm(out, results[0]) : undefined;
    return results.map((type, index) => fromWasm(out[index], type));
  };
  Object.defineProperty(fn, "length", { value: params.length });
  Object.defineProperty(fn, "name", { value: String(funcNames.get(id) ?? id) });
  ids.set(fn, id);
  wrappers.set(`function:${id}`, fn);
  return fn;
}

// Called by the native layer to run a JavaScript import with Wasm arguments.
internal.wasmCallImport = function wasmCallImport(fn, args, params, results) {
  const value = fn(...args.map((arg, index) => fromWasm(arg, params[index])));
  if (results.length === 0) return [];
  if (results.length === 1) return [toWasm(value, results[0])];
  const list = [...value];
  if (list.length !== results.length) throw new TypeError("multi-return length mismatch");
  return list.map((item, index) => toWasm(item, results[index]));
};

// An [EnforceRange] unsigned integer, as V8 checks descriptor properties and arguments.
function unsigned(value, prefix, what, upper = 2 ** 32 - 1) {
  const number = +value;
  if (!Number.isFinite(number)) throw new TypeError(`${prefix}: ${what} must be convertible to a valid number`);
  if (number < 0) throw new TypeError(`${prefix}: ${what} must be non-negative`);
  const result = Math.trunc(number);
  if (result > upper) throw new RangeError(`${prefix}: ${what}: value ${result} is above the upper bound ${upper}`);
  return result;
}

function limits(descriptor, prefix, upper, maximumUpper) {
  if (descriptor.initial === undefined) throw new TypeError(`${prefix}: Property 'initial' is required`);
  const initial = unsigned(descriptor.initial, prefix, "Property 'initial'", upper);
  if (descriptor.maximum === undefined) return { initial, maximum: undefined };
  const maximum = unsigned(descriptor.maximum, prefix, "Property 'maximum'", maximumUpper);
  if (maximum < initial) throw new RangeError(`${prefix}: Property 'maximum': value ${maximum} is below the lower bound ${initial}`);
  return { initial, maximum };
}

function descriptorOf(value, prefix, what) {
  if (value === null || typeof value !== "object") throw new TypeError(`${prefix}: Argument 0 must be a ${what} descriptor`);
  return value;
}

const Memory = defineInterface(
  "Memory",
  function (descriptor) {
    const prefix = "WebAssembly.Memory()";
    const { initial, maximum } = limits(descriptorOf(descriptor, prefix, "memory"), prefix, 65536, 65536);
    if (descriptor.shared) throw internal.unsupported("WebAssembly.Memory with shared: true");
    wrap(Memory, guard("", () => wasm.memoryNew(initial, maximum)), this);
  },
  {
    grow(delta) {
      const prefix = "WebAssembly.Memory.grow()";
      const id = idOf(this, Memory, prefix, "Receiver is not a WebAssembly.Memory");
      return guard("", () => wasm.memoryGrow(id, unsigned(delta, prefix, "Argument 0")));
    },
    get buffer() {
      const buffer = wasm.memoryBuffer(idOf(this, Memory, "WebAssembly.Memory.buffer", "Receiver is not a WebAssembly.Memory"));
      internal.nonDetachable.add(buffer);
      return buffer;
    },
  },
);

function checkIndex(id, index, prefix) {
  const info = wasm.tableInfo(id);
  if (index >= info.size) throw new RangeError(`${prefix}: invalid index ${index} into ${info.element} table of size ${info.size}`);
  return info;
}

function tableValue(args, element, prefix) {
  const value = args.length < 2 ? undefined : args[1];
  if (value === undefined) return element === "externref" ? toWasm(undefined, element) : null;
  if (element === "funcref" && value !== null && !ids.has(value)) {
    const reason = "function-typed object must be null (if nullable) or a Wasm function object";
    throw new TypeError(`${prefix}: Argument 1 is invalid for table: ${reason}`);
  }
  return toWasm(value, element);
}

function tableId(table, prefix) {
  return idOf(table, Table, prefix, "Receiver is not a WebAssembly.Table");
}

const Table = defineInterface(
  "Table",
  function (descriptor) {
    const prefix = "WebAssembly.Table()";
    descriptorOf(descriptor, prefix, "table");
    const element = REFERENCE_TYPES[descriptor.element];
    if (!element) throw new TypeError(`${prefix}: Descriptor property 'element' must be a WebAssembly reference type`);
    const { initial, maximum } = limits(descriptor, prefix, 10000000);
    const init = tableValue(arguments, element, prefix);
    wrap(Table, guard("", () => wasm.tableNew(element, initial, maximum, init)), this);
  },
  {
    get length() {
      return wasm.tableInfo(tableId(this, "WebAssembly.Table.length")).size;
    },
    grow(delta) {
      const prefix = "WebAssembly.Table.grow()";
      const id = tableId(this, prefix);
      const count = unsigned(delta, prefix, "Argument 0");
      const init = tableValue(arguments, wasm.tableInfo(id).element, prefix);
      try {
        return wasm.tableGrow(id, count, init);
      } catch {
        throw new RangeError(`${prefix}: failed to grow table by ${count}`);
      }
    },
    set(index) {
      const prefix = "WebAssembly.Table.set()";
      const id = tableId(this, prefix);
      const at = unsigned(index, prefix, "Argument 0");
      wasm.tableSet(id, at, tableValue(arguments, checkIndex(id, at, prefix).element, prefix));
    },
    get(index) {
      const prefix = "WebAssembly.Table.get()";
      const id = tableId(this, prefix);
      const at = unsigned(index, prefix, "Argument 0");
      const { element } = checkIndex(id, at, prefix);
      return fromWasm(wasm.tableGet(id, at), element);
    },
  },
);

const Global = defineInterface(
  "Global",
  function (descriptor, value) {
    const prefix = "WebAssembly.Global()";
    descriptorOf(descriptor, prefix, "global");
    const mutable = Boolean(descriptor.mutable);
    const type = VALUE_TYPES[descriptor.value];
    if (!type) throw new TypeError(`${prefix}: Descriptor property 'value' must be a WebAssembly type`);
    if (type === "v128") throw new TypeError(`${prefix}: A global of type 'v128' cannot be created in JavaScript`);
    if (value === undefined) value = DEFAULTS[type];
    if (type === "funcref" && value !== null && !ids.has(value)) throw incompatible();
    wrap(Global, wasm.globalNew(type, mutable, toWasm(value, type)), this);
  },
  {
    valueOf() {
      return this.value;
    },
    get value() {
      const id = idOf(this, Global, "get WebAssembly.Global.value", "Receiver is not a WebAssembly.Global");
      return fromWasm(wasm.globalGet(id), wasm.globalInfo(id).value);
    },
    set value(value) {
      const id = idOf(this, Global, "set WebAssembly.Global.value", "Receiver is not a WebAssembly.Global");
      const info = wasm.globalInfo(id);
      if (!info.mutable) throw new TypeError("set WebAssembly.Global.value: Can't set the value of an immutable global.");
      if (info.value === "funcref" && value !== null && !ids.has(value)) throw incompatible();
      wasm.globalSet(id, toWasm(value, info.value));
    },
  },
);

module.exports = {
  ids,
  funcNames,
  CompileError,
  LinkError,
  RuntimeError,
  Memory,
  Table,
  Global,
  defineInterface,
  guard,
  idOf,
  wrap,
  toWasm,
  functionFor,
};
