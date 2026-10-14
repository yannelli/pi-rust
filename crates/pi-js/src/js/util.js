// node:util.
const { inspect, format, formatWithOptions, stylize } = require("internal/inspect");
const { isDeepStrictEqual } = require("internal/deep_equal");
const errors = require("internal/errors");

const kCustomPromisify = Symbol.for("nodejs.util.promisify.custom");

function promisify(original) {
  if (typeof original !== "function") throw new TypeError('The "original" argument must be of type function');
  if (original[kCustomPromisify]) {
    const custom = original[kCustomPromisify];
    if (typeof custom !== "function") throw new TypeError("util.promisify.custom must be a function");
    return Object.defineProperty(custom, kCustomPromisify, { value: custom, configurable: true });
  }
  function promisified(...args) {
    return new Promise((resolve, reject) => {
      Reflect.apply(original, this, [...args, (error, ...values) => {
        if (error) reject(error);
        else resolve(values.length > 1 ? values : values[0]);
      }]);
    });
  }
  Object.setPrototypeOf(promisified, Object.getPrototypeOf(original));
  Object.defineProperty(promisified, kCustomPromisify, { value: promisified, configurable: true });
  return Object.defineProperties(promisified, Object.getOwnPropertyDescriptors(original));
}
promisify.custom = kCustomPromisify;

function callbackify(original) {
  if (typeof original !== "function") throw new TypeError('The "original" argument must be of type function');
  return function callbackified(...args) {
    const callback = args.pop();
    if (typeof callback !== "function") throw new TypeError("The last argument must be of type function");
    Reflect.apply(original, this, args).then(
      (value) => process.nextTick(callback, null, value),
      (reason) => {
        if (!reason) {
          const error = new Error("Promise was rejected with a falsy value");
          error.reason = reason;
          reason = error;
        }
        process.nextTick(callback, reason);
      },
    );
  };
}

const warned = new Set();
function deprecate(fn, message, code) {
  let warnedOnce = false;
  return function deprecated(...args) {
    if (!warnedOnce && !(code && warned.has(code))) {
      warnedOnce = true;
      if (code) warned.add(code);
      process.emitWarning(message, "DeprecationWarning", code);
    }
    return new.target ? Reflect.construct(fn, args, new.target) : Reflect.apply(fn, this, args);
  };
}

function inherits(ctor, superCtor) {
  if (typeof ctor !== "function" || typeof superCtor !== "function" || superCtor.prototype === undefined) {
    throw new TypeError("The constructor and super constructor must be functions with a prototype");
  }
  Object.defineProperty(ctor, "super_", { value: superCtor, writable: true, configurable: true });
  Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}

const tagOf = (value) => Object.prototype.toString.call(value).slice(8, -1);
const TypedArray = Object.getPrototypeOf(Uint8Array);
const types = {
  isPromise: (value) => value instanceof Promise,
  isDate: (value) => value instanceof Date,
  isRegExp: (value) => value instanceof RegExp,
  isMap: (value) => value instanceof Map,
  isSet: (value) => value instanceof Set,
  isWeakMap: (value) => value instanceof WeakMap,
  isWeakSet: (value) => value instanceof WeakSet,
  isMapIterator: (value) => tagOf(value) === "Map Iterator",
  isSetIterator: (value) => tagOf(value) === "Set Iterator",
  isNativeError: (value) => value instanceof Error,
  isAsyncFunction: (value) => typeof value === "function" && value[Symbol.toStringTag] === "AsyncFunction",
  isGeneratorFunction: (value) => typeof value === "function" &&
    (value[Symbol.toStringTag] === "GeneratorFunction" || value[Symbol.toStringTag] === "AsyncGeneratorFunction"),
  isGeneratorObject: (value) => tagOf(value) === "Generator",
  isArrayBuffer: (value) => value instanceof ArrayBuffer,
  isSharedArrayBuffer: (value) => typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer,
  isAnyArrayBuffer: (value) => value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer),
  isArrayBufferView: (value) => ArrayBuffer.isView(value),
  isDataView: (value) => value instanceof DataView,
  isTypedArray: (value) => value instanceof TypedArray,
  isBoxedPrimitive: (value) => value instanceof Number || value instanceof String || value instanceof Boolean ||
    value instanceof BigInt || value instanceof Symbol,
  isNumberObject: (value) => value instanceof Number,
  isStringObject: (value) => value instanceof String,
  isBooleanObject: (value) => value instanceof Boolean,
  isBigIntObject: (value) => value instanceof BigInt,
  isSymbolObject: (value) => value instanceof Symbol,
  isWeakRef: (value) => typeof WeakRef !== "undefined" && value instanceof WeakRef,
  isProxy: (value) => native.util.isProxy(value),
  isExternal: () => false,
  isModuleNamespaceObject: (value) => tagOf(value) === "Module",
};
for (const name of ["Uint8Array", "Uint8ClampedArray", "Uint16Array", "Uint32Array", "Int8Array", "Int16Array",
  "Int32Array", "Float32Array", "Float64Array", "BigInt64Array", "BigUint64Array"]) {
  types[`is${name}`] = (value) => typeof globalThis[name] === "function" && value instanceof globalThis[name];
}

function debuglog(section) {
  const enabled = new RegExp(`^(${String((globalThis.process && process.env.NODE_DEBUG) || "")
    .replace(/[|\\{}()[\]^$+?.]/g, "\\$&").replace(/\*/g, ".*").replace(/,/g, "$|^")})$`, "i").test(section);
  const logger = (...args) => {
    if (enabled) process.stderr.write(`${section.toUpperCase()} ${process.pid}: ${format(...args)}\n`);
  };
  logger.enabled = enabled;
  return logger;
}

function stripVTControlCharacters(text) {
  return String(text).replace(/[\u001B\u009B][[\]()#;?]*(?:(?:(?:[a-zA-Z\d]*(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?\u0007)|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-ntqry=><~]))/g, "");
}

function styleText(styleNames, text) {
  let result = String(text);
  for (const name of Array.isArray(styleNames) ? styleNames : [styleNames]) {
    const codes = inspect.colors[name];
    if (!codes) throw new TypeError(`The argument 'format' must be a valid style. Received ${name}`);
    result = `\u001b[${codes[0]}m${result}\u001b[${codes[1]}m`;
  }
  return result;
}

function aborted(signal, resource) {
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => signal.addEventListener("abort", () => resolve(), { once: true }));
}

const legacy = {
  isArray: Array.isArray,
  isBoolean: (value) => typeof value === "boolean",
  isNull: (value) => value === null,
  isNullOrUndefined: (value) => value === null || value === undefined,
  isNumber: (value) => typeof value === "number",
  isString: (value) => typeof value === "string",
  isSymbol: (value) => typeof value === "symbol",
  isUndefined: (value) => value === undefined,
  isRegExp: types.isRegExp,
  isObject: (value) => value !== null && typeof value === "object",
  isDate: types.isDate,
  isError: (value) => value instanceof Error,
  isFunction: (value) => typeof value === "function",
  isPrimitive: (value) => value === null || (typeof value !== "object" && typeof value !== "function"),
};

function transferableAbortSignal(signal) {
  if (!(signal instanceof AbortSignal)) throw errors.invalidArgType("signal", "an instance of AbortSignal", signal);
  return signal;
}

module.exports = {
  ...legacy,
  inspect,
  format,
  formatWithOptions,
  promisify,
  callbackify,
  deprecate,
  inherits,
  types,
  isDeepStrictEqual,
  debuglog,
  debug: debuglog,
  stripVTControlCharacters,
  styleText,
  aborted,
  toUSVString: (text) => String(text).toWellFormed ? String(text).toWellFormed() : String(text),
  get TextEncoder() {
    return require("internal/web").TextEncoder;
  },
  get TextDecoder() {
    return require("internal/web").TextDecoder;
  },
  parseArgs: (config) => require("internal/parse_args").parseArgs(config),
  getSystemErrorName: errors.getSystemErrorName,
  getSystemErrorMap: () => new Map(errors.getSystemErrorMap()),
  getSystemErrorMessage: errors.getSystemErrorMessage,
  get MIMEType() {
    return require("internal/mime").MIMEType;
  },
  get MIMEParams() {
    return require("internal/mime").MIMEParams;
  },
  transferableAbortController: () => new AbortController(),
  transferableAbortSignal,
  _stylize: stylize,
};
