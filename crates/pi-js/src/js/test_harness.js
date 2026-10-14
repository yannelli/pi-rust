// node:test runner core: tests, suites, hooks, and the root harness. Tests at one level run one
// at a time in declaration order, and each reports start, pass or fail, plan, and diagnostic
// events in Node's shape to the harness sink.
const { AsyncLocalStorage } = require("async_hooks");
const { performance } = require("perf_hooks");

const storage = new AsyncLocalStorage();
const now = () => performance.now();
const elapsed = (start) => Math.round((now() - start) * 1e6) / 1e6;
const CANCELLED = "test did not finish before its parent and was cancelled";

function failure(error, failureType, own) {
  const message = own ? error : error !== null && typeof error === "object" ? (error.message ?? String(error)) : String(error);
  const wrapper = new Error(message);
  wrapper.code = "ERR_TEST_FAILURE";
  wrapper.failureType = failureType;
  wrapper.cause = error;
  Object.defineProperty(wrapper, "stack", { value: `Error [ERR_TEST_FAILURE]: ${message}`, writable: true, configurable: true });
  return wrapper;
}

function isFailure(error) {
  return error instanceof Error && error.code === "ERR_TEST_FAILURE" && typeof error.failureType === "string";
}

function settle(run) {
  try {
    return Promise.resolve(run());
  } catch (error) {
    return Promise.reject(error);
  }
}

// Run a test or hook function; one that takes a second parameter gets a `done` callback.
function invoke(test, fn, ctx) {
  if (fn.length < 2) return settle(() => storage.run(test, () => fn.call(ctx, ctx)));
  return new Promise((resolve, reject) => {
    let called = false;
    const done = (error) => {
      if (called) return;
      called = true;
      if (error) reject(error);
      else resolve();
    };
    let result;
    try {
      result = storage.run(test, () => fn.call(ctx, ctx, done));
    } catch (error) {
      reject(error);
      return;
    }
    if (result && typeof result.then === "function") {
      result.then(undefined, () => {});
      reject(new Error("passed a callback but also returned a Promise"));
    }
  });
}

function withTimeout(timeout, run, failureType) {
  if (!Number.isFinite(timeout)) return settle(run);
  let timer;
  const expired = new Promise((_, reject) => {
    timer = setTimeout(() => reject(failure(`${failureType === "testTimeoutFailure" ? "test" : "hook"} timed out after ${timeout}ms`, failureType, true)), timeout);
    timer.unref?.();
  });
  return Promise.race([settle(run), expired]).finally(() => clearTimeout(timer));
}

class Test {
  constructor({ name, fn, options = {}, parent = null, harness, loc = {} }) {
    this.name = name;
    this.fn = fn || (() => {});
    this.parent = parent;
    this.harness = harness;
    this.nesting = parent ? parent.nesting + 1 : -1;
    this.loc = loc;
    this.reportedType = "test";
    this.skipped = options.skip !== undefined && options.skip !== false;
    this.isTodo = options.todo !== undefined && options.todo !== false;
    this.message = this.skipped ? options.skip : this.isTodo ? options.todo : undefined;
    this.timeout = options.timeout ?? parent?.timeout ?? Infinity;
    this.planned = options.plan ?? null;
    this.assertions = 0;
    this.subtests = [];
    this.queue = [];
    this.active = 0;
    this.hooks = {
      before: [],
      after: [],
      beforeEach: parent ? parent.hooks.beforeEach.slice() : [],
      afterEach: parent ? parent.hooks.afterEach.slice() : [],
      ownAfterEach: 0,
    };
    this.diagnostics = [];
    if (options.only) this.diagnostics.push("'only' and 'runOnly' require the --test-only command-line option.");
    this.passed = false;
    this.error = null;
    this.cancelled = false;
    this.started = false;
    this.finished = false;
    this.controller = new AbortController();
    if (options.signal) {
      if (options.signal.aborted) this.controller.abort(options.signal.reason);
      else options.signal.addEventListener("abort", () => this.controller.abort(options.signal.reason), { once: true });
    }
    this.done = new Promise((resolve) => (this.resolveDone = resolve));
    if (parent) {
      parent.subtests.push(this);
      this.testNumber = parent.subtests.length;
    }
  }

  get fullName() {
    const names = [];
    for (let test = this; test && test.parent; test = test.parent) names.unshift(test.name);
    return names.join(" > ");
  }

  report(type, data) {
    this.harness.report(type, data);
  }

  where() {
    const { file, line, column } = this.loc;
    return file === undefined ? {} : { file, line, column };
  }

  addHook(name, fn, options = {}) {
    const hook = { fn, timeout: options.timeout ?? Infinity };
    if (name === "afterEach") this.hooks.afterEach.splice(this.hooks.ownAfterEach++, 0, hook);
    else this.hooks[name].push(hook);
    // As in Node, a before hook added to a test that is already running runs at once.
    if (name === "before" && (this.parent === null || this.started)) {
      hook.ran = this.runOne(hook, this.harness.contextFor(this));
      hook.ran.catch(() => {});
    }
  }

  async runOne(hook, ctx) {
    try {
      await withTimeout(hook.timeout, () => invoke(this, hook.fn, ctx), "hookFailed");
    } catch (error) {
      throw isFailure(error) ? error : failure(error, "hookFailed");
    }
  }

  async runHook(name, ctx) {
    for (const hook of this.hooks[name]) await this.runOne(hook, ctx);
  }

  async runBefore() {
    const ctx = this.harness.contextFor(this);
    for (const hook of this.hooks.before) {
      hook.ran ??= this.runOne(hook, ctx);
      await hook.ran;
    }
  }

  fail(error) {
    if (this.error !== null) return;
    this.passed = false;
    this.error = error;
  }

  pass() {
    if (this.error === null) this.passed = true;
  }

  // Queue a subtest of a running test; it starts when the earlier ones have finished.
  enqueue(child) {
    this.queue.push(child);
    this.processQueue();
    return child.done;
  }

  processQueue() {
    if (this.parent === null && !this.harness.running) return;
    while (this.active < 1 && this.queue.length > 0) {
      const child = this.queue.shift();
      if (child.finished) continue;
      this.active++;
      child.run().finally(() => {
        this.active--;
        this.processQueue();
      });
    }
    if (this.parent === null && this.active === 0 && this.queue.length === 0) this.harness.idle();
  }

  start() {
    this.started = true;
    this.startTime = now();
    this.report("test:start", { nesting: this.nesting, name: this.name, ...this.where() });
  }

  async run() {
    if (this.finished) return;
    this.start();
    const parent = this.parent;
    const ctx = this.harness.contextFor(this);
    if (this.skipped) {
      this.pass();
      return this.complete();
    }
    const afterHooks = async () => {
      if (parent.hooks.afterEach.length > 0) await parent.runHook("afterEach", ctx);
      if (this.hooks.after.length > 0) await this.runHook("after", ctx);
    };
    try {
      if (parent.hooks.before.length > 0) await parent.runBefore();
      if (parent.hooks.beforeEach.length > 0) await parent.runHook("beforeEach", ctx);
      await withTimeout(this.timeout, () => invoke(this, this.fn, ctx), "testTimeoutFailure");
      if (this.planned !== null && this.assertions !== this.planned) {
        throw failure(`plan expected ${this.planned} assertions but received ${this.assertions}`, "testCodeFailure", true);
      }
      this.pass();
      await afterHooks();
    } catch (error) {
      if (!isFailure(error)) this.fail(failure(error, "testCodeFailure"));
      else if (error.failureType === "testTimeoutFailure") this.cancel(error);
      else this.fail(error);
      try {
        await afterHooks();
      } catch {}
    }
    this.harness.resetMocks(this);
    this.controller.abort();
    await this.postRun();
  }

  async postRun() {
    if (this.finished) return;
    if (this.subtests.some((subtest) => !subtest.finished)) await new Promise((resolve) => setImmediate(resolve));
    let failed = 0;
    for (const subtest of this.subtests) {
      if (!subtest.finished) subtest.cancel(failure(CANCELLED, "cancelledByParent", true));
      if (!subtest.passed && !subtest.isTodo) failed++;
    }
    if (failed > 0 && (this.passed || this.parent === null)) {
      this.fail(failure(`${failed} subtest${failed > 1 ? "s" : ""} failed`, "subtestsFailed", true));
    }
    this.complete();
  }

  cancel(error) {
    if (this.finished) return;
    if (!this.started) this.start();
    for (const subtest of this.subtests) {
      if (!subtest.finished) subtest.cancel(failure(CANCELLED, "cancelledByParent", true));
    }
    this.cancelled = true;
    this.fail(error);
    this.complete();
  }

  complete() {
    if (this.finished) return;
    this.finished = true;
    const where = this.where();
    if (this.subtests.length > 0) this.report("test:plan", { nesting: this.nesting + 1, count: this.subtests.length, ...where });
    const details = { duration_ms: elapsed(this.startTime), type: this.reportedType };
    if (!this.passed) details.error = this.error;
    const directive = {};
    if (this.skipped) directive.skip = typeof this.message === "string" ? this.message : true;
    else if (this.isTodo) directive.todo = typeof this.message === "string" ? this.message : true;
    const data = { name: this.name, nesting: this.nesting, testNumber: this.testNumber, details, ...directive, ...where };
    this.report(this.passed ? "test:pass" : "test:fail", data);
    for (const message of this.diagnostics) this.report("test:diagnostic", { nesting: this.nesting, message, ...where });
    this.harness.count(this);
    this.resolveDone();
  }
}

class Suite extends Test {
  constructor(options) {
    super(options);
    this.reportedType = "suite";
    if (this.skipped) return;
    try {
      const ctx = this.harness.contextFor(this);
      const result = storage.run(this, () => this.fn.call(ctx, ctx));
      if (result && typeof result.then === "function") this.build = Promise.resolve(result);
    } catch (error) {
      this.buildError = error;
    }
  }

  async run() {
    if (this.finished) return;
    this.start();
    if (this.skipped) {
      this.pass();
      return this.complete();
    }
    try {
      if (this.build) await this.build;
      if (this.buildError !== undefined) throw this.buildError;
    } catch (error) {
      this.fail(failure(error, "testCodeFailure"));
      return this.postRun();
    }
    try {
      if (this.parent.hooks.before.length > 0) await this.parent.runBefore();
      if (this.hooks.before.length > 0) await this.runBefore();
      for (const subtest of this.subtests) {
        if (this.finished) return;
        await subtest.run();
      }
      if (this.hooks.after.length > 0) await this.runHook("after", this.harness.contextFor(this));
      this.pass();
    } catch (error) {
      this.fail(isFailure(error) ? error : failure(error, "testCodeFailure"));
    }
    this.controller.abort();
    await this.postRun();
  }
}

// One run of tests: the root test, counters, and the event sink.
class Harness {
  constructor(sink, contextFor, resetMocks) {
    this.sink = sink;
    this.contextFor = contextFor;
    this.resetMocks = resetMocks;
    this.counters = { tests: 0, suites: 0, passed: 0, failed: 0, cancelled: 0, skipped: 0, todo: 0, topLevel: 0 };
    this.root = new Test({ name: "<root>", harness: this });
    this.running = false;
    this.finished = false;
    this.startTime = now();
    this.whenFinished = new Promise((resolve) => (this.resolveFinished = resolve));
  }

  report(type, data) {
    this.sink(type, data);
  }

  count(test) {
    const counters = this.counters;
    if (test.nesting === 0) counters.topLevel++;
    if (test.reportedType === "suite") {
      counters.suites++;
      return;
    }
    if (test.skipped) counters.skipped++;
    else if (test.isTodo) counters.todo++;
    else if (test.cancelled) counters.cancelled++;
    else if (!test.passed) counters.failed++;
    else counters.passed++;
    counters.tests++;
  }

  // Top-level tests start after the code that declares them, as in Node.
  add(child) {
    this.root.queue.push(child);
    if (this.running) {
      this.root.processQueue();
    } else if (!this.scheduled) {
      this.scheduled = true;
      setImmediate(() => {
        this.running = true;
        this.root.processQueue();
      });
    }
  }

  idle() {
    if (this.waiting || this.finished) return;
    this.waiting = true;
    internal.onIdle(() => {
      this.waiting = false;
      if (this.root.queue.length > 0) return this.root.processQueue();
      const pending = failure("Promise resolution is still pending but the event loop has already resolved", "cancelledByParent", true);
      for (const test of this.root.subtests) if (test.started && !test.finished) test.cancel(pending);
      this.finish();
    });
  }

  async finish() {
    if (this.finished) return;
    this.finished = true;
    const root = this.root;
    try {
      if (root.hooks.after.length > 0) await root.runHook("after", this.contextFor(root));
    } catch (error) {
      root.fail(error);
    }
    const counters = this.counters;
    this.report("test:plan", { nesting: 0, count: root.subtests.length });
    const duration = elapsed(this.startTime);
    for (const [label, value] of [
      ["tests", counters.tests],
      ["suites", counters.suites],
      ["pass", counters.passed],
      ["fail", counters.failed],
      ["cancelled", counters.cancelled],
      ["skipped", counters.skipped],
      ["todo", counters.todo],
      ["duration_ms", duration],
    ]) {
      this.report("test:diagnostic", { nesting: 0, message: `${label} ${value}` });
    }
    const success = counters.failed === 0 && counters.cancelled === 0 && root.error === null;
    this.report("test:summary", { success, counts: { ...counters }, duration_ms: duration, file: undefined });
    this.resolveFinished(success);
  }
}

module.exports = { Test, Suite, Harness, storage, failure };
