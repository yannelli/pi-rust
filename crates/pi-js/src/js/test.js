// node:test: test, describe, it, hooks, mock, and run. Tests declared at the top level run after
// the main module, and the results print in TAP form (spec on a terminal or with
// --test-reporter=spec) when the event loop runs out of work, as Node does at exit.
const assert = require("assert");
const path = require("path");
const { Readable } = require("stream");
const { Test, Suite, Harness, storage } = require("internal/test_harness");
const { MockTracker } = require("internal/test_mock");
const reporters = require("test/reporters");
const { validate } = require("internal/errors");

let globalHarness = null;
let loadingHarness = null;
const customAssertions = new Map();
const ASSERTIONS = [
  "deepEqual",
  "deepStrictEqual",
  "doesNotMatch",
  "doesNotReject",
  "doesNotThrow",
  "equal",
  "fail",
  "ifError",
  "match",
  "notDeepEqual",
  "notDeepStrictEqual",
  "notEqual",
  "notStrictEqual",
  "ok",
  "partialDeepStrictEqual",
  "rejects",
  "strictEqual",
  "throws",
];

function callSite() {
  for (const line of String(new Error().stack).split("\n").slice(1)) {
    const match = /\(?([^\s()]+):(\d+):(\d+)\)?\s*$/.exec(line);
    if (match && !match[1].startsWith("node:") && match[1] !== "native") {
      return { file: match[1], line: Number(match[2]), column: Number(match[3]) };
    }
  }
  return {};
}

class TestContext {
  #test;
  #assert;
  constructor(test) {
    this.#test = test;
  }
  get name() {
    return this.#test.name;
  }
  get fullName() {
    return this.#test.fullName;
  }
  get filePath() {
    return this.#test.loc.file;
  }
  get signal() {
    return this.#test.controller.signal;
  }
  get error() {
    return this.#test.error;
  }
  get passed() {
    return this.#test.passed;
  }
  get mock() {
    this.#test.mockTracker ??= new MockTracker();
    return this.#test.mockTracker;
  }
  get assert() {
    if (this.#assert) return this.#assert;
    const test = this.#test;
    const methods = {};
    for (const name of ASSERTIONS) {
      if (typeof assert[name] !== "function") continue;
      methods[name] = (...args) => {
        test.assertions++;
        return assert[name](...args);
      };
    }
    for (const name of ["snapshot", "fileSnapshot"]) {
      methods[name] = () => {
        throw internal.unsupported(`node:test t.assert.${name}`);
      };
    }
    for (const [name, fn] of customAssertions) {
      methods[name] = (...args) => {
        test.assertions++;
        return fn.apply(this, args);
      };
    }
    this.#assert = methods;
    return methods;
  }
  diagnostic(message) {
    this.#test.diagnostics.push(String(message));
  }
  plan(count) {
    validate.integer(count, "count", 1);
    this.#test.planned = count;
  }
  skip(message) {
    this.#test.skipped = true;
    this.#test.message = message;
  }
  todo(message) {
    this.#test.isTodo = true;
    this.#test.message = message;
  }
  runOnly(value) {
    validate.boolean(value, "shouldRunOnlyTests");
  }
  test(...args) {
    return create(Test, args, {}, this.#test);
  }
  before(fn, options) {
    this.#test.addHook("before", fn, options);
  }
  after(fn, options) {
    this.#test.addHook("after", fn, options);
  }
  beforeEach(fn, options) {
    this.#test.addHook("beforeEach", fn, options);
  }
  afterEach(fn, options) {
    this.#test.addHook("afterEach", fn, options);
  }
  async waitFor(condition, options = {}) {
    validate.function(condition, "condition");
    const { interval = 50, timeout = 1000 } = options;
    const start = Date.now();
    for (;;) {
      try {
        return await condition();
      } catch (error) {
        if (Date.now() - start >= timeout) throw error;
      }
      await new Promise((resolve) => setTimeout(resolve, interval));
    }
  }
}

class SuiteContext {
  #suite;
  constructor(suite) {
    this.#suite = suite;
  }
  get name() {
    return this.#suite.name;
  }
  get fullName() {
    return this.#suite.fullName;
  }
  get filePath() {
    return this.#suite.loc.file;
  }
  get signal() {
    return this.#suite.controller.signal;
  }
}

function contextFor(test) {
  test.context ??= test.reportedType === "suite" ? new SuiteContext(test) : new TestContext(test);
  return test.context;
}

function resetMocks(test) {
  test.mockTracker?.reset();
}

function reporterName() {
  const args = process.execArgv || [];
  for (let i = 0; i < args.length; i++) {
    if (args[i].startsWith("--test-reporter=")) return args[i].slice("--test-reporter=".length);
    if (args[i] === "--test-reporter") return args[i + 1];
  }
  return process.stdout.isTTY ? "spec" : "tap";
}

function printingHarness() {
  const formatter = (reporters.formatters[reporterName()] || reporters.formatters.tap)();
  let started = false;
  const harness = new Harness(
    (type, data) => {
      let text = started ? "" : formatter.head();
      started = true;
      text += formatter.format({ type, data });
      if (type === "test:summary") text += formatter.flush();
      if (text) process.stdout.write(text);
    },
    contextFor,
    resetMocks,
  );
  harness.whenFinished.then((success) => {
    if (!success) process.exitCode = 1;
  });
  return harness;
}

function currentParent() {
  const store = storage.getStore();
  if (store instanceof Test && !store.finished && !store.harness.finished) return store;
  if (loadingHarness) return loadingHarness.root;
  if (globalHarness === null || globalHarness.finished) globalHarness = printingHarness();
  return globalHarness.root;
}

function parseArgs(name, options, fn) {
  if (typeof name === "function") [fn, options, name] = [name, undefined, undefined];
  else if (name !== null && typeof name === "object") [fn, options, name] = [options, name, undefined];
  else if (typeof options === "function") [fn, options] = [options, undefined];
  if (options === undefined || options === null) options = {};
  validate.object(options, "options");
  if (typeof fn !== "function") fn = undefined;
  if (typeof name !== "string" || name === "") name = fn?.name || "<anonymous>";
  return { name, options, fn };
}

function create(Kind, args, overrides, parent = currentParent()) {
  const { name, options, fn } = parseArgs(...args);
  const child = new Kind({ name, fn, options: { ...options, ...overrides }, parent, harness: parent.harness, loc: callSite() });
  if (parent.parent === null) {
    parent.harness.add(child);
    return Kind === Suite ? Promise.resolve() : child.done;
  }
  if (parent.reportedType === "suite") return Promise.resolve();
  return parent.enqueue(child);
}

function variants(Kind) {
  const run = (...args) => create(Kind, args, {});
  run.skip = (...args) => create(Kind, args, { skip: true });
  run.todo = (...args) => create(Kind, args, { todo: true });
  run.only = (...args) => create(Kind, args, { only: true });
  return run;
}

const test = variants(Test);
const describe = variants(Suite);

function hook(name) {
  return function (fn, options) {
    validate.function(fn, "fn");
    currentParent().addHook(name, fn, options);
  };
}

class TestsStream extends Readable {
  constructor() {
    super({ objectMode: true });
  }
  _read() {}
  event(type, data) {
    this.emit(type, data);
    this.push({ type, data });
  }
  compose(reporter) {
    if (typeof reporter === "function") return Readable.from(reporter(this));
    return this.pipe(reporter);
  }
}

// Run test files in this runtime, one after another, and stream their events.
function run(options = {}) {
  validate.object(options, "options");
  const files = options.files ?? [];
  validate.array(files, "options.files");
  const stream = new TestsStream();
  const harness = new Harness((type, data) => stream.event(type, data), contextFor, resetMocks);
  (async () => {
    if (typeof options.setup === "function") await options.setup(stream);
    for (const file of files) {
      const full = path.resolve(file);
      loadingHarness = harness;
      try {
        if (/\.m[jt]s$/.test(full)) {
          await import(require("url").pathToFileURL(full).href);
        } else {
          delete require("module")._cache[full];
          internal.createRequire(full)(full);
        }
      } catch (error) {
        loadingHarness = null;
        harness.add(new Test({ name: full, fn: () => Promise.reject(error), parent: harness.root, harness, loc: { file: full } }));
      } finally {
        loadingHarness = null;
      }
    }
    if (harness.root.subtests.length === 0) harness.finish();
    else harness.root.processQueue();
    await harness.whenFinished;
    stream.push(null);
  })().catch((error) => stream.destroy(error));
  return stream;
}

const snapshot = {
  setDefaultSnapshotSerializers(serializers) {
    validate.array(serializers, "serializers");
  },
  setResolveSnapshotPath(fn) {
    validate.function(fn, "fn");
  },
};

module.exports = test;
Object.assign(test, {
  test,
  it: test,
  describe,
  suite: describe,
  before: hook("before"),
  after: hook("after"),
  beforeEach: hook("beforeEach"),
  afterEach: hook("afterEach"),
  mock: new MockTracker(),
  run,
  snapshot,
  assert: {
    register(name, fn) {
      validate.string(name, "name");
      validate.function(fn, "fn");
      customAssertions.set(name, fn);
    },
  },
});
