// internal/fs_ops: each fs operation as a native request plus a result mapper, shared by the
// sync, callback, and promise APIs.
const utils = require("internal/fs_utils");
const { toPath, toFd, getOptions, flagString, parseMode, decode, dataToLatin1, Stats, makeEntries, checkBigint } = utils;

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

const toStats = (raw) => new Stats(raw);

const ops = {
  access: (path, mode = 0) => ({ request: { op: "access", path: toPath(path), mode } }),
  exists: (path) => ({ request: { op: "exists", path: toPath(path) } }),
  readFile(path, options) {
    options = getOptions(options, { flag: "r" });
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
    checkBigint(options);
    return { request: { op: "stat", path: toPath(path), follow: true }, map: toStats };
  },
  lstat(path, options) {
    checkBigint(options);
    return { request: { op: "stat", path: toPath(path), follow: false }, map: toStats };
  },
  fstat(fd, options) {
    checkBigint(options);
    return { request: { op: "fstat", fd: toFd(fd) }, map: toStats };
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
    if (options.filter) throw internal.unsupported("fs.cp with a filter option");
    const request = {
      op: "cp",
      from: toPath(from, "src"),
      to: toPath(to, "dest"),
      recursive: !!options.recursive,
      force: options.force !== false,
      errorOnExist: !!options.errorOnExist,
    };
    return { request };
  },
  realpath: (path, options) => ({ request: { op: "realpath", path: toPath(path) }, map: encoded(options) }),
  readlink: (path, options) => ({ request: { op: "readlink", path: toPath(path) }, map: encoded(options) }),
  symlink: (target, path, type) => ({ request: { op: "symlink", target: toPath(target, "target"), path: toPath(path), kind: type || undefined } }),
  link: (from, to) => ({ request: { op: "link", from: toPath(from, "existingPath"), to: toPath(to, "newPath") } }),
  chmod: (path, mode) => ({ request: { op: "chmod", path: toPath(path), mode: parseMode(mode) } }),
  chown: (path, uid, gid) => ({ request: { op: "chown", path: toPath(path), uid, gid, follow: true } }),
  lchown: (path, uid, gid) => ({ request: { op: "chown", path: toPath(path), uid, gid, follow: false } }),
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

for (const name of ["lchmod", "fchmod", "fchown", "lutimes", "statfs", "openAsBlob"]) {
  ops[name] = () => {
    throw internal.unsupported(`fs.${name}`);
  };
}

function runSync(spec) {
  const result = utils.sync(spec.request, spec.data);
  return spec.map ? spec.map(result) : result;
}

function runAsync(spec) {
  const promise = utils.async(spec.request, spec.data);
  return spec.map ? promise.then(spec.map) : promise;
}

module.exports = { ops, runSync, runAsync, toSeconds };
