// internal/fs_ops: each fs operation as a native request plus a result mapper, shared by the
// sync, callback, and promise APIs.
const utils = require("internal/fs_utils");
const { toPath, toFd, getOptions, flagString, parseMode, decode, dataToLatin1, Stats, makeEntries, statsMapper } = utils;

function toSeconds(time) {
  if (time instanceof Date) return time.getTime() / 1000;
  if (typeof time === "number") return time;
  if (typeof time === "string" && time !== "" && !Number.isNaN(Number(time))) return Number(time);
  throw utils.invalidArg("time", "of type number or an instance of Date", time);
}

function encoded(options) {
  const { encoding } = getOptions(options, { encoding: "utf8" });
  return (text) => (encoding === "buffer" ? require("buffer").Buffer.from(text) : text);
}


const ops = {
  access: (path, mode = 0) => ({ request: { op: "access", path: toPath(path), mode } }),
  exists: (path) => ({ request: { op: "exists", path: toPath(path) } }),
  readFile(path, options) {
    options = getOptions(options, { flag: "r" });
    if (path === 0) return { custom: () => native.pipe.readStdinSync(), map: (bytes) => decode(bytes, options.encoding) };
    const request = typeof path === "number" ? { op: "readFile", fd: toFd(path) } : { op: "readFile", path: toPath(path) };
    return { request, map: (bytes) => decode(bytes, options.encoding) };
  },
  writeFile(path, data, options) {
    options = getOptions(options, { encoding: "utf8", mode: 0o666, flag: "w" });
    const request = typeof path === "number"
      ? { op: "writeFile", fd: toFd(path), flag: "w" }
      : { op: "writeFile", path: toPath(path), flag: flagString(options.flag, "w"), mode: parseMode(options.mode, 0o666) };
    return { request, data: dataToLatin1(data, options.encoding) };
  },
  appendFile(path, data, options) {
    return ops.writeFile(path, data, getOptions(options, { encoding: "utf8", mode: 0o666, flag: "a" }));
  },
  stat(path, options) {
    return { request: { op: "stat", path: toPath(path), follow: true }, map: statsMapper(options) };
  },
  lstat(path, options) {
    return { request: { op: "stat", path: toPath(path), follow: false }, map: statsMapper(options) };
  },
  fstat(fd, options) {
    return { request: { op: "fstat", fd: toFd(fd) }, map: statsMapper(options) };
  },
  readdir(path, options) {
    options = getOptions(options, { encoding: "utf8" });
    const directory = toPath(path);
    return { request: { op: "readdir", path: directory, recursive: !!options.recursive }, map: (entries) => makeEntries(entries, directory, options) };
  },
  mkdir(path, options) {
    if (typeof options === "number" || typeof options === "string") options = { mode: options };
    options = options || {};
    return { request: { op: "mkdir", path: toPath(path), recursive: !!options.recursive, mode: parseMode(options.mode, 0o777) } };
  },
  rmdir: (path, options) => ({ request: { op: "rmdir", path: toPath(path), recursive: !!(options && options.recursive) } }),
  rm(path, options) {
    options = options || {};
    return { request: { op: "rm", path: toPath(path), recursive: !!options.recursive, force: !!options.force } };
  },
  unlink: (path) => ({ request: { op: "unlink", path: toPath(path) } }),
  rename: (from, to) => ({ request: { op: "rename", from: toPath(from, "oldPath"), to: toPath(to, "newPath") } }),
  copyFile: (from, to, mode = 0) => ({ request: { op: "copyFile", from: toPath(from, "src"), to: toPath(to, "dest"), flags: mode } }),
  cp(from, to, options) {
    options = options || {};
    const request = {
      op: "cp",
      from: toPath(from, "src"),
      to: toPath(to, "dest"),
      recursive: !!options.recursive,
      force: options.force !== false,
      errorOnExist: !!options.errorOnExist,
    };
    if (options.filter === undefined) return { request };
    if (typeof options.filter !== "function") throw utils.invalidArg("options.filter", "of type function", options.filter);
    return { walk: () => Object.assign(cpWalk(request.from, request.to, request, options), { options }) };
  },
  realpath: (path, options) => ({ request: { op: "realpath", path: toPath(path) }, map: encoded(options) }),
  readlink: (path, options) => ({ request: { op: "readlink", path: toPath(path) }, map: encoded(options) }),
  symlink: (target, path, type) => ({ request: { op: "symlink", target: toPath(target, "target"), path: toPath(path), kind: type || undefined } }),
  link: (from, to) => ({ request: { op: "link", from: toPath(from, "existingPath"), to: toPath(to, "newPath") } }),
  chmod: (path, mode) => ({ request: { op: "chmod", path: toPath(path), mode: parseMode(mode) } }),
  chown: (path, uid, gid) => ({ request: { op: "chown", path: toPath(path), ...ids(uid, gid), follow: true } }),
  lchown: (path, uid, gid) => ({ request: { op: "chown", path: toPath(path), ...ids(uid, gid), follow: false } }),
  utimes: (path, atime, mtime) => ({ request: { op: "utimes", path: toPath(path), atime: toSeconds(atime), mtime: toSeconds(mtime) } }),
  futimes: (fd, atime, mtime) => ({ request: { op: "futimes", fd: toFd(fd), atime: toSeconds(atime), mtime: toSeconds(mtime) } }),
  truncate(path, len = 0) {
    if (typeof path === "number") return ops.ftruncate(path, len);
    return { request: { op: "truncate", path: toPath(path), len } };
  },
  ftruncate: (fd, len = 0) => ({ request: { op: "ftruncate", fd: toFd(fd), len } }),
  fsync: (fd) => ({ request: { op: "fsync", fd: toFd(fd) } }),
  fdatasync: (fd) => ({ request: { op: "fsync", fd: toFd(fd) } }),
  mkdtemp(prefix, options) {
    if (typeof prefix !== "string") throw utils.invalidArg("prefix", "of type string", prefix);
    return { request: { op: "mkdtemp", prefix }, map: encoded(options) };
  },
  open(path, flags, mode) {
    return { request: { op: "open", path: toPath(path), flags: flagString(flags, "r"), mode: parseMode(mode, 0o666) } };
  },
  close: (fd) => ({ request: { op: "close", fd: toFd(fd) } }),
};

function ids(uid, gid) {
  const { validate } = require("internal/errors");
  validate.integer(uid, "uid", -1, 4294967295);
  validate.integer(gid, "gid", -1, 4294967295);
  return { uid: uid >>> 0, gid: gid >>> 0 };
}

class StatFs {
  constructor(type, bsize, blocks, bfree, bavail, files, ffree) {
    Object.assign(this, { type, bsize, blocks, bfree, bavail, files, ffree });
  }
}

Object.assign(ops, {
  fchmod: (fd, mode) => ({ request: { op: "fchmod", fd: toFd(fd), mode: parseMode(mode) } }),
  fchown: (fd, uid, gid) => ({ request: { op: "fchown", fd: toFd(fd), ...ids(uid, gid) } }),
  lutimes: (path, atime, mtime) => ({ request: { op: "lutimes", path: toPath(path), atime: toSeconds(atime), mtime: toSeconds(mtime) } }),
  statfs(path, options) {
    const convert = options && options.bigint ? BigInt : Number;
    return { request: { op: "statfs", path: toPath(path) }, map: (fields) => new StatFs(...fields.map((field) => convert(field))) };
  },
});
// Node defines lchmod only where O_SYMLINK exists, which is macOS.
if (utils.constants.O_SYMLINK !== undefined) {
  ops.lchmod = (path, mode) => ({ request: { op: "lchmod", path: toPath(path), mode: parseMode(mode) } });
}

// fs.cp with a filter: ask the filter about each item, then copy files one by one.
function* cpWalk(from, to, request, options) {
  if (!(yield ["filter", from, to])) return;
  const path = require("path");
  const stats = yield ["op", options.dereference ? ops.stat(from) : ops.lstat(from)];
  if (!stats.isDirectory() || !request.recursive) {
    yield ["op", { request: { ...request, from, to, recursive: false } }];
    return;
  }
  yield ["op", ops.mkdir(to, { recursive: true })];
  for (const name of yield ["op", ops.readdir(from)]) yield* cpWalk(path.join(from, name), path.join(to, name), request, options);
}

function filterResult(value, sync) {
  if (sync && value && typeof value.then === "function") {
    throw Object.assign(new TypeError('Expected boolean to be returned from the "filter" function but got instance of Promise.'), { code: "ERR_INVALID_RETURN_VALUE" });
  }
  return value;
}

function runSync(spec) {
  if (spec.custom) return spec.map(spec.custom());
  if (spec.walk) {
    const steps = spec.walk();
    for (let input, step; !(step = steps.next(input)).done;) {
      const [kind, a, b] = step.value;
      input = kind === "filter" ? filterResult(steps.options.filter(a, b), true) : runSync(a);
    }
    return undefined;
  }
  const result = utils.sync(spec.request, spec.data);
  return spec.map ? spec.map(result) : result;
}

async function runWalk(spec) {
  const steps = spec.walk();
  for (let input, step; !(step = steps.next(input)).done;) {
    const [kind, a, b] = step.value;
    input = kind === "filter" ? await steps.options.filter(a, b) : await runAsync(a);
  }
}

function runAsync(spec) {
  if (spec.custom) return Promise.resolve().then(() => spec.map(spec.custom()));
  if (spec.walk) return runWalk(spec);
  const promise = utils.async(spec.request, spec.data);
  return spec.map ? promise.then(spec.map) : promise;
}

module.exports = { ops, runSync, runAsync, toSeconds };
