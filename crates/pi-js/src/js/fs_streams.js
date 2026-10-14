// internal/fs_streams: ReadStream and WriteStream on file descriptors, plus polling watchers.
const { Buffer } = require("buffer");
const EventEmitter = require("events");
const path = require("path");
const common = require("internal/streams_common");
const { Readable } = require("internal/streams_readable");
const { Writable } = require("internal/streams_writable");
const utils = require("internal/fs_utils");
const { ops, runAsync, runSync } = require("internal/fs_ops");

// Open the file before the first read or write, emitting 'open' and 'ready' like Node.
function openOnConstruct(stream, flags, mode) {
  common.construct(stream, (callback) => {
    if (stream.fd !== null && stream.fd !== undefined) {
      callback();
      return;
    }
    runAsync(ops.open(stream.path, flags, mode)).then(
      (fd) => {
        stream.fd = fd;
        stream.emit("open", fd);
        stream.emit("ready");
        callback();
      },
      (error) => callback(error),
    );
  });
}

function closeOnDestroy(stream, error, callback) {
  if (stream.fd === null || stream.fd === undefined || !stream.autoClose) {
    callback(error);
    return;
  }
  const fd = stream.fd;
  stream.fd = null;
  runAsync(ops.close(fd)).then(() => callback(error), (failure) => callback(error || failure));
}

function ReadStream(file, options) {
  if (!(this instanceof ReadStream)) return new ReadStream(file, options);
  options = utils.getOptions(options, {});
  Readable.call(this, {
    highWaterMark: options.highWaterMark || 64 * 1024,
    encoding: options.encoding,
    emitClose: options.emitClose !== false,
    autoDestroy: options.autoClose !== false,
    signal: options.signal,
  });
  this.fd = options.fd === undefined ? null : options.fd;
  this.path = this.fd === null ? utils.toPath(file) : file;
  this.flags = options.flags === undefined ? "r" : options.flags;
  this.mode = options.mode === undefined ? 0o666 : options.mode;
  this.start = options.start;
  this.end = options.end === undefined ? Infinity : options.end;
  this.pos = this.start;
  this.bytesRead = 0;
  this.autoClose = options.autoClose !== false;
  openOnConstruct(this, this.flags, this.mode);
}
Object.setPrototypeOf(ReadStream.prototype, Readable.prototype);
Object.setPrototypeOf(ReadStream, Readable);
Object.defineProperty(ReadStream.prototype, "pending", {
  get() {
    return this.fd === null;
  },
});
ReadStream.prototype._read = function _read(size) {
  let length = size;
  if (this.pos !== undefined && this.end !== Infinity) length = Math.min(size, this.end - this.pos + 1);
  else if (this.end !== Infinity) length = Math.min(size, this.end - this.bytesRead + 1);
  if (length <= 0) {
    this.push(null);
    return;
  }
  const position = this.pos === undefined ? null : this.pos;
  utils.async({ op: "read", fd: this.fd, length, position }).then(
    (bytes) => {
      if (this.pos !== undefined) this.pos += bytes.length;
      this.bytesRead += bytes.length;
      this.push(bytes.length ? Buffer.from(bytes.buffer, bytes.byteOffset, bytes.length) : null);
    },
    (error) => this.destroy(error),
  );
};
ReadStream.prototype._destroy = function _destroy(error, callback) {
  closeOnDestroy(this, error, callback);
};
ReadStream.prototype.close = function close(callback) {
  if (typeof callback === "function") common.eos(this, callback);
  this.destroy();
};

function WriteStream(file, options) {
  if (!(this instanceof WriteStream)) return new WriteStream(file, options);
  options = utils.getOptions(options, {});
  Writable.call(this, {
    highWaterMark: options.highWaterMark,
    decodeStrings: true,
    defaultEncoding: options.encoding || "utf8",
    emitClose: options.emitClose !== false,
    autoDestroy: options.autoClose !== false,
    signal: options.signal,
  });
  this.fd = options.fd === undefined ? null : options.fd;
  this.path = this.fd === null ? utils.toPath(file) : file;
  this.flags = options.flags === undefined ? "w" : options.flags;
  this.mode = options.mode === undefined ? 0o666 : options.mode;
  this.start = options.start;
  this.pos = this.start;
  this.bytesWritten = 0;
  this.autoClose = options.autoClose !== false;
  this.flush = !!options.flush;
  openOnConstruct(this, this.flags, this.mode);
}
Object.setPrototypeOf(WriteStream.prototype, Writable.prototype);
Object.setPrototypeOf(WriteStream, Writable);
Object.defineProperty(WriteStream.prototype, "pending", {
  get() {
    return this.fd === null;
  },
});
WriteStream.prototype._write = function _write(chunk, encoding, callback) {
  const position = this.pos === undefined ? null : this.pos;
  utils.async({ op: "write", fd: this.fd, position }, internal.bytesToLatin1(chunk)).then(
    (written) => {
      if (this.pos !== undefined) this.pos += written;
      this.bytesWritten += written;
      callback();
    },
    callback,
  );
};
WriteStream.prototype._final = function _final(callback) {
  if (!this.flush || this.fd === null) {
    callback();
    return;
  }
  runAsync(ops.fsync(this.fd)).then(() => callback(), callback);
};
WriteStream.prototype._destroy = function _destroy(error, callback) {
  closeOnDestroy(this, error, callback);
};
WriteStream.prototype.close = function close(callback) {
  if (typeof callback === "function") common.eos(this, callback);
  if (!this.writableEnded) this.end();
  else this.destroy();
};
WriteStream.prototype.destroySoon = WriteStream.prototype.end;

// fs.watch on native file events; `persistent: false` lets the process exit while watching.
class FSWatcher extends EventEmitter {
  #id = null;
  #persistent = true;
  constructor(target, options, listener) {
    super();
    if (listener) this.on("change", listener);
    const encoding = options.encoding || "utf8";
    this.#id = native.watch.watch(target, !!options.recursive, (eventType, value) => {
      if (eventType === "error") {
        this.emit("error", value);
        this.close();
        return;
      }
      const name = encoding === "buffer" ? require("buffer").Buffer.from(value) : value;
      this.emit("change", eventType, name);
    });
    this.#persistent = options.persistent !== false;
    native.watch.ref(this.#id, this.#persistent);
    if (options.signal) {
      if (options.signal.aborted) process.nextTick(() => this.close());
      else options.signal.addEventListener("abort", () => this.close(), { once: true });
    }
  }
  close() {
    if (this.#id === null) return;
    native.watch.close(this.#id);
    this.#id = null;
    process.nextTick(() => this.emit("close"));
  }
  ref() {
    if (this.#id !== null) native.watch.ref(this.#id, true);
    return this;
  }
  unref() {
    if (this.#id !== null) native.watch.ref(this.#id, false);
    return this;
  }
}

function watch(filename, options, listener) {
  if (typeof options === "function") [listener, options] = [options, {}];
  options = utils.getOptions(options, { persistent: true });
  return new FSWatcher(utils.toPath(filename), options, listener);
}

const EMPTY_STATS = { dev: 0, ino: 0, mode: 0, nlink: 0, uid: 0, gid: 0, rdev: 0, size: 0, blksize: 0, blocks: 0, atimeMs: 0, mtimeMs: 0, ctimeMs: 0, birthtimeMs: 0 };
const fileWatchers = new Map();

// A missing file reads as all-zero stats, as Node's watchFile reports it.
function currentStats(filename) {
  try {
    return runSync(ops.stat(filename));
  } catch {
    return new utils.Stats(EMPTY_STATS);
  }
}

class StatWatcher extends EventEmitter {
  #timer;
  constructor(filename, interval, persistent) {
    super();
    let previous = currentStats(filename);
    this.#timer = setInterval(() => {
      const current = currentStats(filename);
      if (current.mtimeMs !== previous.mtimeMs || current.size !== previous.size || current.ino !== previous.ino) {
        this.emit("change", current, previous);
      }
      previous = current;
    }, interval);
    if (!persistent) this.#timer.unref();
  }
  stop() {
    clearInterval(this.#timer);
    this.emit("stop");
  }
  ref() {
    this.#timer.ref();
    return this;
  }
  unref() {
    this.#timer.unref();
    return this;
  }
}

function watchFile(filename, options, listener) {
  if (typeof options === "function") [listener, options] = [options, {}];
  options = { interval: 5007, persistent: true, ...options };
  const target = path.resolve(utils.toPath(filename));
  let watcher = fileWatchers.get(target);
  if (!watcher) {
    watcher = new StatWatcher(target, options.interval, options.persistent);
    fileWatchers.set(target, watcher);
  }
  watcher.on("change", listener);
  return watcher;
}

function unwatchFile(filename, listener) {
  const target = path.resolve(utils.toPath(filename));
  const watcher = fileWatchers.get(target);
  if (!watcher) return;
  if (listener) watcher.removeListener("change", listener);
  else watcher.removeAllListeners("change");
  if (watcher.listenerCount("change") === 0) {
    watcher.stop();
    fileWatchers.delete(target);
  }
}

module.exports = { ReadStream, WriteStream, FSWatcher, StatWatcher, watch, watchFile, unwatchFile };
