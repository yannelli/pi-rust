// internal/fs_realpath: fs.realpathSync and fs.realpath, ported from Node 22's lib/fs.js (LICENSE-node).
// Only links are replaced, so other components keep the caller's spelling; `.native` is the OS realpath.
const pathModule = require("path");
const utils = require("internal/fs_utils");
const { ops, runSync, runAsync } = require("internal/fs_ops");

const isWindows = internal.config.platform === "win32";
const windowsRoot = /^(?:[a-zA-Z]:|[\\/]{2}[^\\/]+[\\/][^\\/]+)?[\\/]*/;

function splitRoot(path) {
  if (isWindows) return windowsRoot.exec(path)[0];
  for (let i = 0; i < path.length; ++i) {
    if (path[i] !== "/") return path.slice(0, i);
  }
  return path;
}

function nextPart(path, start) {
  if (!isWindows) return path.indexOf("/", start);
  for (let i = start; i < path.length; ++i) {
    if (path[i] === "\\" || path[i] === "/") return i;
  }
  return -1;
}

function resolvedInput(path) {
  if (path && typeof path === "object" && typeof path.href === "string" && path.protocol !== undefined) path = utils.toPath(path);
  if (typeof path !== "string") path += "";
  return pathModule.resolve(utils.toPath(path));
}

function encode(result, options) {
  const { encoding } = options;
  if (!encoding || encoding === "utf8") return result;
  const bytes = require("buffer").Buffer.from(result);
  return encoding === "buffer" ? bytes : bytes.toString(encoding);
}

// Yields each fs request and receives its result. `followed` stands for Node's shared `statValues`:
// the last non-bigint stat, of a followed link or a Windows root, which a known part after a restart reads.
function* walk(path) {
  const seenLinks = new Map();
  const knownHard = new Set();
  let followed;
  let current = splitRoot(path);
  let base = current;
  let pos = current.length;
  if (isWindows) {
    followed = yield ops.lstat(base);
    knownHard.add(base);
  }
  while (pos < path.length) {
    const next = nextPart(path, pos);
    const previous = current;
    if (next === -1) {
      const last = path.slice(pos);
      current += last;
      base = previous + last;
      pos = path.length;
    } else {
      current += path.slice(pos, next + 1);
      base = previous + path.slice(pos, next);
      pos = next + 1;
    }
    if (knownHard.has(base)) {
      if (followed.isFIFO() || followed.isSocket()) break;
      continue;
    }
    const stats = yield ops.lstat(base, { bigint: true });
    if (!stats.isSymbolicLink()) {
      knownHard.add(base);
      continue;
    }
    const id = isWindows ? null : `${stats.dev.toString(32)}:${stats.ino.toString(32)}`;
    let target = id !== null && seenLinks.has(id) ? seenLinks.get(id) : null;
    if (target === null) {
      followed = yield ops.stat(base);
      target = yield ops.readlink(base);
    }
    const resolvedLink = pathModule.resolve(previous, target);
    if (id !== null) seenLinks.set(id, target);
    path = pathModule.resolve(resolvedLink, path.slice(pos));
    current = base = splitRoot(path);
    pos = current.length;
    if (isWindows && !knownHard.has(base)) {
      followed = yield ops.lstat(base);
      knownHard.add(base);
    }
  }
  return path;
}

function walkSync(path) {
  const steps = walk(path);
  let step = steps.next();
  while (!step.done) step = steps.next(runSync(step.value));
  return step.value;
}

async function walkAsync(path) {
  const steps = walk(path);
  let step = steps.next();
  while (!step.done) step = steps.next(await runAsync(step.value));
  return step.value;
}

function realpathSync(path, options) {
  options = utils.getOptions(options);
  return encode(walkSync(resolvedInput(path)), options);
}

function realpath(path, options, callback) {
  if (typeof options === "function") callback = options;
  else if (typeof callback !== "function") throw utils.invalidArg("cb", "of type function", callback);
  options = utils.getOptions(options);
  walkAsync(resolvedInput(path)).then(
    (result) => process.nextTick(() => callback(null, encode(result, options))),
    (error) => process.nextTick(callback, error),
  );
}

module.exports = { realpathSync, realpath };
