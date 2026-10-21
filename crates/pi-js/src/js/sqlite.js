// node:sqlite on rusqlite with the bundled SQLite. A StatementSync keeps its SQL text, and the
// connection prepares it through a statement cache on each call.
const { Buffer } = require("buffer");
const { codeError } = require("internal/errors");
const nativeSqlite = native.sqlite;

const kInternal = Symbol("kInternal");
const kHandle = Symbol("kHandle");
const kConfig = Symbol("kConfig");
const kStatement = Symbol("kStatement");
const BIG = 1;
const ARRAYS = 2;
const BARE = 4;
const UNKNOWN = 8;
const MAX_I64 = 2n ** 63n - 1n;
const MIN_I64 = -(2n ** 63n);

process.emitWarning("SQLite is an experimental feature and might change at any time", "ExperimentalWarning");

function argType(name, expected) {
  return codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" argument must be ${expected}.`);
}

function invalidState(message) {
  return codeError(Error, "ERR_INVALID_STATE", message);
}

function checkBoolean(value, name) {
  if (value !== undefined && typeof value !== "boolean") throw argType(name, "a boolean");
}

function databasePath(path) {
  let text;
  if (typeof path === "string") text = path;
  else if (path instanceof Uint8Array) text = Buffer.from(path).toString();
  else if (path && typeof path === "object" && typeof path.href === "string" && path.protocol === "file:") text = path.href;
  if (text === undefined || text.includes("\0")) throw argType("path", "a string, Uint8Array, or URL without null bytes");
  return text;
}

function handleOf(db) {
  if (!db[kHandle]) throw invalidState("database is not open");
  return db[kHandle];
}

function DatabaseSync(path, options) {
  if (!new.target) throw codeError(TypeError, "ERR_CONSTRUCT_CALL_REQUIRED", "Cannot call constructor without `new`");
  const location = databasePath(path);
  if (options !== undefined && (options === null || typeof options !== "object")) throw argType("options", "an object");
  const opts = options || {};
  for (const name of ["open", "readOnly", "enableForeignKeyConstraints", "enableDoubleQuotedStringLiterals", "allowExtension"]) {
    checkBoolean(opts[name], `options.${name}`);
  }
  for (const name of ["readBigInts", "returnArrays", "allowBareNamedParameters", "allowUnknownNamedParameters"]) {
    checkBoolean(opts[name], `options.${name}`);
  }
  if (opts.timeout !== undefined && !Number.isInteger(opts.timeout)) throw argType("options.timeout", "an integer");
  let flags = BARE;
  if (opts.readBigInts) flags |= BIG;
  if (opts.returnArrays) flags |= ARRAYS;
  if (opts.allowBareNamedParameters === false) flags &= ~BARE;
  if (opts.allowUnknownNamedParameters) flags |= UNKNOWN;
  Object.defineProperty(this, kConfig, {
    value: {
      location,
      readOnly: opts.readOnly === true,
      foreignKeys: opts.enableForeignKeyConstraints !== false,
      doubleQuoted: opts.enableDoubleQuotedStringLiterals === true,
      allowExtension: opts.allowExtension === true,
      timeout: opts.timeout || 0,
      flags,
    },
  });
  Object.defineProperty(this, kHandle, { value: 0, writable: true });
  Object.defineProperty(this, "isOpen", { get: () => this[kHandle] !== 0, enumerable: true, configurable: true });
  Object.defineProperty(this, "isTransaction", {
    get: () => nativeSqlite.isTransaction(handleOf(this)),
    enumerable: true,
    configurable: true,
  });
  if (opts.open !== false) this.open();
}

DatabaseSync.prototype.open = function open() {
  if (this[kHandle]) throw invalidState("database is already open");
  const config = this[kConfig];
  this[kHandle] = nativeSqlite.open(config.location, config.readOnly, config.foreignKeys, config.doubleQuoted, config.timeout);
};

DatabaseSync.prototype.close = function close() {
  nativeSqlite.close(handleOf(this));
  this[kHandle] = 0;
};

DatabaseSync.prototype.exec = function exec(sql) {
  const handle = handleOf(this);
  if (typeof sql !== "string") throw argType("sql", "a string");
  nativeSqlite.exec(handle, sql);
};

DatabaseSync.prototype.prepare = function prepare(sql) {
  const handle = handleOf(this);
  if (typeof sql !== "string") throw argType("sql", "a string");
  nativeSqlite.prepare(handle, sql);
  return new StatementSync(kInternal, this, handle, sql);
};

DatabaseSync.prototype.location = function location(dbName = "main") {
  const handle = handleOf(this);
  if (typeof dbName !== "string") throw argType("dbName", "a string");
  return nativeSqlite.location(handle, dbName);
};

function functionFlags(options) {
  for (const name of ["useBigIntArguments", "varargs", "deterministic", "directOnly"]) {
    checkBoolean(options[name], `options.${name}`);
  }
  return (options.deterministic ? 1 : 0) | (options.directOnly ? 2 : 0) | (options.useBigIntArguments ? 4 : 0);
}

DatabaseSync.prototype.function = function sqlFunction(name, options, fn) {
  const handle = handleOf(this);
  if (typeof name !== "string") throw argType("name", "a string");
  if (arguments.length < 3) [options, fn] = [{}, options];
  else if (options === null || typeof options !== "object") throw argType("options", "an object");
  const flags = functionFlags(options);
  if (typeof fn !== "function") throw argType("function", "a function");
  nativeSqlite.function(handle, name, fn, options.varargs ? -1 : fn.length, flags);
};

DatabaseSync.prototype.aggregate = function aggregate(name, options) {
  const handle = handleOf(this);
  if (typeof name !== "string") throw argType("name", "a string");
  if (options === null || typeof options !== "object") throw argType("options", "an object");
  if (options.start === undefined) throw argType("options.start", "a function or a primitive value");
  if (typeof options.step !== "function") throw argType("options.step", "a function");
  if (options.result !== undefined && typeof options.result !== "function") throw argType("options.result", "a function");
  if (options.inverse !== undefined && typeof options.inverse !== "function") throw argType("options.inverse", "a function");
  const flags = functionFlags(options);
  const length = Math.max(options.step.length, options.inverse ? options.inverse.length : 0);
  const arity = options.varargs ? -1 : Math.max(0, length - 1);
  nativeSqlite.aggregate(handle, name, [options.start, options.step, options.result, options.inverse], arity, flags);
};

DatabaseSync.prototype.enableLoadExtension = function enableLoadExtension(allow) {
  if (typeof allow !== "boolean") throw argType("allow", "a boolean");
  handleOf(this);
  if (!this[kConfig].allowExtension) {
    throw invalidState("Cannot enable extension loading because it was disabled at database creation.");
  }
  if (allow) throw internal.unsupported("sqlite extension loading");
};

DatabaseSync.prototype.loadExtension = function loadExtension() {
  handleOf(this);
  if (!this[kConfig].allowExtension) throw invalidState("extension loading is not allowed");
  throw internal.unsupported("sqlite extension loading");
};

DatabaseSync.prototype.createSession = function createSession() {
  handleOf(this);
  throw internal.unsupported("sqlite sessions (createSession)");
};

DatabaseSync.prototype.applyChangeset = function applyChangeset() {
  handleOf(this);
  throw internal.unsupported("sqlite changesets (applyChangeset)");
};

DatabaseSync.prototype[Symbol.dispose] = function dispose() {
  if (this[kHandle]) this.close();
};

function bindable(value) {
  if (typeof value === "bigint" && (value > MAX_I64 || value < MIN_I64)) {
    throw codeError(TypeError, "ERR_INVALID_ARG_VALUE", "BigInt value is too large to bind.");
  }
  if (ArrayBuffer.isView(value) && !(value instanceof Uint8Array)) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  return value;
}

function StatementSync(token, db, handle, sql) {
  if (token !== kInternal) throw codeError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
  Object.defineProperty(this, kStatement, { value: { db, handle, sql, flags: db[kConfig].flags, last: [[], undefined], cursor: null } });
  Object.defineProperty(this, "sourceSQL", { get: () => sql, enumerable: true, configurable: true });
  Object.defineProperty(this, "expandedSQL", {
    get: () => {
      const state = this[kStatement];
      return nativeSqlite.query(liveHandle(state), sql, state.last[0], state.last[1], state.flags, 3);
    },
    enumerable: true,
    configurable: true,
  });
}

function liveHandle(state) {
  if (state.db[kHandle] !== state.handle) throw invalidState("statement has been finalized");
  return state.handle;
}

function prepareArgs(state, args) {
  const handle = liveHandle(state);
  resetCursor(state);
  let named;
  let positional = args;
  const first = args[0];
  if (first !== null && typeof first === "object" && !ArrayBuffer.isView(first)) {
    named = {};
    for (const key of Object.keys(first)) named[key] = bindable(first[key]);
    positional = args.slice(1);
  }
  positional = positional.map(bindable);
  state.last = [positional, named];
  return [handle, positional, named];
}

function execute(statement, args, mode) {
  const state = statement[kStatement];
  const [handle, positional, named] = prepareArgs(state, args);
  return nativeSqlite.query(handle, state.sql, positional, named, state.flags, mode);
}

StatementSync.prototype.all = function all(...args) {
  return execute(this, args, 0);
};

StatementSync.prototype.get = function get(...args) {
  return execute(this, args, 1);
};

StatementSync.prototype.run = function run(...args) {
  return execute(this, args, 2);
};

const IteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));

// Node keeps one SQLite statement per StatementSync, so its iterators share a cursor: running
// the statement again resets it, and the next step starts over with the latest bindings.
function resetCursor(state) {
  if (state.cursor !== null && state.db[kHandle] === state.handle) nativeSqlite.finish(state.handle, state.cursor);
  state.cursor = null;
}

function StatementSyncIterator(statement) {
  Object.defineProperty(this, kStatement, { value: { statement, done: false } });
}
const done = () => ({ __proto__: null, done: true, value: null });
StatementSyncIterator.prototype = Object.create(IteratorPrototype);
StatementSyncIterator.prototype.next = function next() {
  const iterator = this[kStatement];
  const state = iterator.statement;
  const handle = liveHandle(state);
  if (iterator.done) return done();
  state.cursor ??= nativeSqlite.iterate(handle, state.sql, state.last[0], state.last[1], state.flags);
  let value;
  try {
    value = nativeSqlite.step(handle, state.cursor, state.flags);
  } catch (error) {
    state.cursor = null;
    throw error;
  }
  if (value === undefined) {
    state.cursor = null;
    iterator.done = true;
    return done();
  }
  return { __proto__: null, done: false, value };
};
StatementSyncIterator.prototype.return = function iteratorReturn() {
  const iterator = this[kStatement];
  liveHandle(iterator.statement);
  iterator.done = true;
  resetCursor(iterator.statement);
  return done();
};
Object.defineProperty(StatementSyncIterator.prototype, "constructor", { value: StatementSyncIterator, writable: true, configurable: true });

StatementSync.prototype.iterate = function iterate(...args) {
  const state = this[kStatement];
  const [handle, positional, named] = prepareArgs(state, args);
  state.cursor = nativeSqlite.iterate(handle, state.sql, positional, named, state.flags);
  return new StatementSyncIterator(state);
};

StatementSync.prototype.columns = function columns() {
  const state = this[kStatement];
  return nativeSqlite.columns(liveHandle(state), state.sql);
};

function setFlag(name, flag) {
  return function (enabled) {
    if (typeof enabled !== "boolean") throw argType(name, "a boolean");
    const state = this[kStatement];
    liveHandle(state);
    state.flags = enabled ? state.flags | flag : state.flags & ~flag;
  };
}
StatementSync.prototype.setAllowBareNamedParameters = setFlag("allowBareNamedParameters", BARE);
StatementSync.prototype.setAllowUnknownNamedParameters = setFlag("enabled", UNKNOWN);
StatementSync.prototype.setReadBigInts = setFlag("readBigInts", BIG);
StatementSync.prototype.setReturnArrays = setFlag("returnArrays", ARRAYS);

function backup(sourceDb, path, options = {}) {
  if (!(sourceDb instanceof DatabaseSync)) throw argType("sourceDb", "an object");
  const target = databasePath(path);
  if (options === null || typeof options !== "object") throw argType("options", "an object");
  const { source = "main", target: targetName = "main", rate = 100, progress } = options;
  if (typeof source !== "string") throw argType("options.source", "a string");
  if (typeof targetName !== "string") throw argType("options.target", "a string");
  if (!Number.isInteger(rate) || rate < -(2 ** 31) || rate >= 2 ** 31) throw argType("options.rate", "an integer");
  if (progress !== undefined && typeof progress !== "function") throw argType("options.progress", "a function");
  const handle = handleOf(sourceDb);
  return new Promise((resolve, reject) => {
    setImmediate(() => {
      try {
        resolve(nativeSqlite.backup(handle, target, source, targetName, rate, progress));
      } catch (error) {
        reject(error);
      }
    });
  });
}

const constants = {
  SQLITE_CHANGESET_OMIT: 0,
  SQLITE_CHANGESET_REPLACE: 1,
  SQLITE_CHANGESET_ABORT: 2,
  SQLITE_CHANGESET_DATA: 1,
  SQLITE_CHANGESET_NOTFOUND: 2,
  SQLITE_CHANGESET_CONFLICT: 3,
  SQLITE_CHANGESET_CONSTRAINT: 4,
  SQLITE_CHANGESET_FOREIGN_KEY: 5,
};

module.exports = { DatabaseSync, StatementSync, constants, backup };
