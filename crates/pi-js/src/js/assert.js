// node:assert and the strict variant.
const { inspect } = require("internal/inspect");
const { isDeepEqual, isDeepStrictEqual } = require("internal/deep_equal");

class AssertionError extends Error {
  constructor(options = {}) {
    const { message, actual, expected, operator } = options;
    super(message !== undefined ? String(message) : `${inspect(actual)} ${operator} ${inspect(expected)}`);
    this.name = "AssertionError";
    this.code = "ERR_ASSERTION";
    this.actual = actual;
    this.expected = expected;
    this.operator = operator;
    this.generatedMessage = message === undefined;
    if (typeof options.stackStartFn === "function" && Error.captureStackTrace) Error.captureStackTrace(this, options.stackStartFn);
  }
}

const OPERATOR_TEXT = {
  "==": "Expected values to be loosely equal:",
  "!=": "Expected \"actual\" to be loosely unequal to:",
  strictEqual: "Expected values to be strictly equal:",
  notStrictEqual: "Expected \"actual\" to be strictly unequal to:",
  deepEqual: "Expected values to be loosely deep-equal:",
  notDeepEqual: "Expected \"actual\" not to be loosely deep-equal to:",
  deepStrictEqual: "Expected values to be strictly deep-equal:",
  notDeepStrictEqual: "Expected \"actual\" not to be strictly deep-equal to:",
};

// Node's message shapes: `1 == 2` for loose checks, a header plus values otherwise.
function generatedMessage(actual, expected, operator) {
  if (operator === "==" || operator === "!=") return `${inspect(actual)} ${operator} ${inspect(expected)}`;
  const header = OPERATOR_TEXT[operator] || operator;
  if (operator.startsWith("not")) return `${header} ${inspect(expected)}`;
  return `${header}\n\n${inspect(actual)} !== ${inspect(expected)}\n`;
}

function fail(actual, expected, message, operator, stackStartFn) {
  if (message instanceof Error) throw message;
  const text = message === undefined ? generatedMessage(actual, expected, operator) : message;
  throw new AssertionError({ message: text, actual, expected, operator, stackStartFn });
}

function innerOk(stackStartFn, argumentCount, value, message) {
  if (value) return;
  let text = message;
  if (argumentCount === 0) text = "No value argument passed to `assert.ok()`";
  else if (message === undefined) text = `The expression evaluated to a falsy value:\n\n  ${inspect(value)}\n`;
  if (text instanceof Error) throw text;
  const error = new AssertionError({ message: text, actual: value, expected: true, operator: "==", stackStartFn });
  error.generatedMessage = message === undefined;
  throw error;
}

function ok(...args) {
  innerOk(ok, args.length, ...args);
}

const assert = function assert(...args) {
  innerOk(assert, args.length, ...args);
};

function comparison(name, operator, test) {
  const check = function (actual, expected, message) {
    if (!test(actual, expected)) fail(actual, expected, message, operator, check);
  };
  Object.defineProperty(check, "name", { value: name });
  return check;
}

const equal = comparison("equal", "==", (a, b) => a == b || (Number.isNaN(a) && Number.isNaN(b))); // eslint-disable-line eqeqeq
const notEqual = comparison("notEqual", "!=", (a, b) => !(a == b || (Number.isNaN(a) && Number.isNaN(b)))); // eslint-disable-line eqeqeq
const strictEqual = comparison("strictEqual", "strictEqual", Object.is);
const notStrictEqual = comparison("notStrictEqual", "notStrictEqual", (a, b) => !Object.is(a, b));
const deepEqual = comparison("deepEqual", "deepEqual", isDeepEqual);
const notDeepEqual = comparison("notDeepEqual", "notDeepEqual", (a, b) => !isDeepEqual(a, b));
const deepStrictEqual = comparison("deepStrictEqual", "deepStrictEqual", isDeepStrictEqual);
const notDeepStrictEqual = comparison("notDeepStrictEqual", "notDeepStrictEqual", (a, b) => !isDeepStrictEqual(a, b));

// Does `actual` (a thrown value) satisfy `expected` (a class, RegExp, validator, or object)?
function matchesExpected(actual, expected, operator, message, stackStartFn) {
  if (expected === undefined) return;
  if (typeof expected === "function") {
    if (expected.prototype !== undefined && actual instanceof expected) return;
    if (Error.isPrototypeOf(expected) || expected === Error) throw actual;
    if (expected.call({}, actual) === true) return;
    throw new AssertionError({ message: message || `The ${operator === "rejects" ? "validation" : "error validation"} function is expected to return "true". Received false`, actual, expected, operator, stackStartFn });
  }
  if (expected instanceof RegExp) {
    if (expected.test(String(actual))) return;
    throw new AssertionError({ message: message || `The input did not match the regular expression ${expected}. Input:\n\n${inspect(String(actual))}\n`, actual, expected, operator, stackStartFn });
  }
  for (const key of Object.keys(expected)) {
    const want = expected[key];
    const got = actual === null || actual === undefined ? undefined : actual[key];
    const same = want instanceof RegExp && typeof got === "string" ? want.test(got) : isDeepStrictEqual(got, want);
    if (!same) {
      throw new AssertionError({ message: message || `Expected values to be strictly deep-equal:\n+ actual - expected\n\n  ${key}: ${inspect(got)} !== ${inspect(want)}`, actual, expected, operator, stackStartFn });
    }
  }
}

const NO_EXCEPTION = Symbol("no exception");
function caught(fn) {
  try {
    fn();
  } catch (error) {
    return error;
  }
  return NO_EXCEPTION;
}

function throws(fn, expected, message) {
  if (typeof fn !== "function") throw Object.assign(new TypeError('The "fn" argument must be of type function'), { code: "ERR_INVALID_ARG_TYPE" });
  if (typeof expected === "string") [message, expected] = [expected, undefined];
  const actual = caught(fn);
  if (actual === NO_EXCEPTION) {
    fail(undefined, expected, message || "Missing expected exception.", "throws", throws);
  }
  matchesExpected(actual, expected, "throws", message, throws);
}

function doesNotThrow(fn, expected, message) {
  if (typeof expected === "string") [message, expected] = [expected, undefined];
  const actual = caught(fn);
  if (actual === NO_EXCEPTION) return;
  if (expected === undefined || (typeof expected === "function" && actual instanceof expected)) {
    fail(actual, expected, `Got unwanted exception${message ? `: ${message}` : "."}\nActual message: "${actual && actual.message}"`, "doesNotThrow", doesNotThrow);
  }
  throw actual;
}

async function settle(promiseOrFn) {
  const promise = typeof promiseOrFn === "function" ? promiseOrFn() : promiseOrFn;
  if (!promise || typeof promise.then !== "function") {
    throw Object.assign(new TypeError('The "promiseFn" argument must be a Promise or a function returning one'), { code: "ERR_INVALID_RETURN_VALUE" });
  }
  try {
    await promise;
  } catch (error) {
    return error;
  }
  return NO_EXCEPTION;
}

async function rejects(promiseOrFn, expected, message) {
  if (typeof expected === "string") [message, expected] = [expected, undefined];
  const actual = await settle(promiseOrFn);
  if (actual === NO_EXCEPTION) fail(undefined, expected, message || "Missing expected rejection.", "rejects", rejects);
  matchesExpected(actual, expected, "rejects", message, rejects);
}

async function doesNotReject(promiseOrFn, expected, message) {
  if (typeof expected === "string") [message, expected] = [expected, undefined];
  const actual = await settle(promiseOrFn);
  if (actual === NO_EXCEPTION) return;
  fail(actual, expected, `Got unwanted rejection${message ? `: ${message}` : "."}\nActual message: "${actual && actual.message}"`, "doesNotReject", doesNotReject);
}

function match(string, regexp, message) {
  if (typeof string !== "string" || !regexp.test(string)) {
    fail(string, regexp, message || `The input did not match the regular expression ${regexp}. Input:\n\n${inspect(string)}\n`, "match", match);
  }
}

function doesNotMatch(string, regexp, message) {
  if (typeof string !== "string" || regexp.test(string)) {
    fail(string, regexp, message || `The input was expected to not match the regular expression ${regexp}. Input:\n\n${inspect(string)}\n`, "doesNotMatch", doesNotMatch);
  }
}

function ifError(value) {
  if (value === null || value === undefined) return;
  let message = "ifError got unwanted exception: ";
  message += value && typeof value.message === "string" ? value.message : inspect(value);
  const error = new AssertionError({ message, actual: value, expected: null, operator: "ifError", stackStartFn: ifError });
  error.origin = value;
  throw error;
}

function failAssert(message = "Failed") {
  if (message instanceof Error) throw message;
  throw new AssertionError({ message, operator: "fail", stackStartFn: failAssert });
}

// Deprecated in Node but still shipped: counts calls to wrapped functions.
class CallTracker {
  #contexts = new Set();
  #tracked = new WeakMap();
  calls(fn, expected = 1) {
    if (typeof fn === "number") [fn, expected] = [() => {}, fn];
    else if (fn === undefined) fn = () => {};
    else if (typeof fn !== "function") throw Object.assign(new TypeError(`The "fn" argument must be of type function. Received ${typeof fn}`), { code: "ERR_INVALID_ARG_TYPE" });
    if (!Number.isInteger(expected) || expected < 1 || expected > 4294967295) {
      throw Object.assign(new RangeError(`The value of "expected" is out of range. It must be >= 1 && <= 4294967295. Received ${expected}`), { code: "ERR_OUT_OF_RANGE" });
    }
    const context = { expected, actual: 0, calls: [], stackTrace: new Error(), name: fn.name || "calls" };
    const contexts = this.#contexts;
    const tracked = new Proxy(fn, {
      apply(target, thisArg, args) {
        context.actual++;
        context.calls.push(Object.freeze({ thisArg, arguments: args }));
        if (context.actual === context.expected) contexts.delete(context);
        else if (context.actual === context.expected + 1) contexts.add(context);
        return Reflect.apply(target, thisArg, args);
      },
    });
    contexts.add(context);
    this.#tracked.set(tracked, context);
    return tracked;
  }
  #context(fn) {
    const context = this.#tracked.get(fn);
    if (!context) throw Object.assign(new TypeError(`The argument 'fn' must be a tracked function. Received ${inspect(fn)}`), { code: "ERR_INVALID_ARG_VALUE" });
    return context;
  }
  getCalls(fn) {
    return this.#context(fn).calls.slice();
  }
  reset(fn) {
    const reset = (context) => {
      context.actual = 0;
      context.calls = [];
      this.#contexts.add(context);
    };
    if (fn === undefined) for (const context of this.#contexts) reset(context);
    else reset(this.#context(fn));
  }
  report() {
    return [...this.#contexts].filter((context) => context.actual !== context.expected).map((context) => ({
      message: `Expected the ${context.name} function to be executed ${context.expected} time(s) but was executed ${context.actual} time(s).`,
      actual: context.actual,
      expected: context.expected,
      operator: context.name,
      stack: context.stackTrace,
    }));
  }
  verify() {
    const errors = this.report();
    if (errors.length === 0) return;
    const message = errors.length === 1 ? errors[0].message : "Functions were not called the expected number of times";
    throw Object.assign(new AssertionError({ message }), { details: errors });
  }
}

Object.assign(assert, {
  ok,
  fail: failAssert,
  equal,
  notEqual,
  strictEqual,
  notStrictEqual,
  deepEqual,
  notDeepEqual,
  deepStrictEqual,
  notDeepStrictEqual,
  throws,
  doesNotThrow,
  rejects,
  doesNotReject,
  match,
  doesNotMatch,
  ifError,
  AssertionError,
  CallTracker,
});

const strict = function strict(...args) {
  innerOk(strict, args.length, ...args);
};
Object.assign(strict, assert, { equal: strictEqual, notEqual: notStrictEqual, deepEqual: deepStrictEqual, notDeepEqual: notDeepStrictEqual });
strict.strict = strict;
assert.strict = strict;

module.exports = assert;
