// node:path, ported from Node's lib/path.js. The win32 flavor lives in internal/path_win32.
const SLASH = 47;
const DOT = 46;

function validateString(value, name) {
  if (typeof value !== "string") {
    const error = new TypeError(`The "${name}" argument must be of type string. Received ${value === null ? "null" : typeof value}`);
    error.code = "ERR_INVALID_ARG_TYPE";
    throw error;
  }
}

function isPosixSeparator(code) {
  return code === SLASH;
}

// Resolve "." and ".." segments; shared with win32.
function normalizeString(path, allowAboveRoot, separator, isSeparator) {
  let res = "";
  let lastSegmentLength = 0;
  let lastSlash = -1;
  let dots = 0;
  let code = 0;
  for (let i = 0; i <= path.length; ++i) {
    if (i < path.length) code = path.charCodeAt(i);
    else if (isSeparator(code)) break;
    else code = SLASH;
    if (isSeparator(code)) {
      if (lastSlash === i - 1 || dots === 1) {
        // Empty segment or ".": nothing to add.
      } else if (dots === 2) {
        if (res.length < 2 || lastSegmentLength !== 2 || res.charCodeAt(res.length - 1) !== DOT || res.charCodeAt(res.length - 2) !== DOT) {
          if (res.length > 2) {
            const lastSlashIndex = res.lastIndexOf(separator);
            if (lastSlashIndex === -1) {
              res = "";
              lastSegmentLength = 0;
            } else {
              res = res.slice(0, lastSlashIndex);
              lastSegmentLength = res.length - 1 - res.lastIndexOf(separator);
            }
            lastSlash = i;
            dots = 0;
            continue;
          } else if (res.length !== 0) {
            res = "";
            lastSegmentLength = 0;
            lastSlash = i;
            dots = 0;
            continue;
          }
        }
        if (allowAboveRoot) {
          res += res.length > 0 ? `${separator}..` : "..";
          lastSegmentLength = 2;
        }
      } else {
        if (res.length > 0) res += `${separator}${path.slice(lastSlash + 1, i)}`;
        else res = path.slice(lastSlash + 1, i);
        lastSegmentLength = i - lastSlash - 1;
      }
      lastSlash = i;
      dots = 0;
    } else if (code === DOT && dots !== -1) {
      ++dots;
    } else {
      dots = -1;
    }
  }
  return res;
}

function formatExt(ext) {
  return ext ? `${ext[0] === "." ? "" : "."}${ext}` : "";
}

function format(sep, pathObject) {
  if (pathObject === null || typeof pathObject !== "object") {
    throw new TypeError(`The "pathObject" argument must be of type object. Received ${pathObject === null ? "null" : typeof pathObject}`);
  }
  const dir = pathObject.dir || pathObject.root;
  const base = pathObject.base || `${pathObject.name || ""}${formatExt(pathObject.ext)}`;
  if (!dir) return base;
  return dir === pathObject.root ? `${dir}${base}` : `${dir}${sep}${base}`;
}

function posixCwd() {
  const cwd = native.process.cwd();
  if (internal.config.platform !== "win32") return cwd;
  const slashed = cwd.replace(/\\/g, "/");
  return slashed.slice(slashed.indexOf("/"));
}

// Shared tail of basename/extname/parse: scan back from the end for the last segment.
function scanSegment(path, start, isSeparator) {
  let startDot = -1;
  let startPart = start;
  let end = -1;
  let matchedSlash = true;
  let preDotState = 0;
  for (let i = path.length - 1; i >= start; --i) {
    const code = path.charCodeAt(i);
    if (isSeparator(code)) {
      if (!matchedSlash) {
        startPart = i + 1;
        break;
      }
      continue;
    }
    if (end === -1) {
      matchedSlash = false;
      end = i + 1;
    }
    if (code === DOT) {
      if (startDot === -1) startDot = i;
      else if (preDotState !== 1) preDotState = 1;
    } else if (startDot !== -1) {
      preDotState = -1;
    }
  }
  const noExt = startDot === -1 || preDotState === 0 || (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1);
  return { startPart, end, startDot: noExt ? -1 : startDot };
}

function basenameWith(path, suffix, start, isSeparator) {
  let end = -1;
  let matchedSlash = true;
  let begin = start;
  if (suffix !== undefined && suffix.length > 0 && suffix.length <= path.length - start) {
    if (suffix === path) return "";
    let extIdx = suffix.length - 1;
    let firstNonSlashEnd = -1;
    for (let i = path.length - 1; i >= start; --i) {
      const code = path.charCodeAt(i);
      if (isSeparator(code)) {
        if (!matchedSlash) {
          begin = i + 1;
          break;
        }
      } else {
        if (firstNonSlashEnd === -1) {
          matchedSlash = false;
          firstNonSlashEnd = i + 1;
        }
        if (extIdx >= 0) {
          if (code === suffix.charCodeAt(extIdx)) {
            if (--extIdx === -1) end = i;
          } else {
            extIdx = -1;
            end = firstNonSlashEnd;
          }
        }
      }
    }
    if (begin === end) end = firstNonSlashEnd;
    else if (end === -1) end = path.length;
    return path.slice(begin, end);
  }
  for (let i = path.length - 1; i >= start; --i) {
    if (isSeparator(path.charCodeAt(i))) {
      if (!matchedSlash) {
        begin = i + 1;
        break;
      }
    } else if (end === -1) {
      matchedSlash = false;
      end = i + 1;
    }
  }
  return end === -1 ? "" : path.slice(begin, end);
}

const posix = {
  sep: "/",
  delimiter: ":",
  resolve(...args) {
    let resolvedPath = "";
    let resolvedAbsolute = false;
    for (let i = args.length - 1; i >= -1 && !resolvedAbsolute; i--) {
      const path = i >= 0 ? args[i] : posixCwd();
      validateString(path, `paths[${i}]`);
      if (path.length === 0) continue;
      resolvedPath = `${path}/${resolvedPath}`;
      resolvedAbsolute = path.charCodeAt(0) === SLASH;
    }
    resolvedPath = normalizeString(resolvedPath, !resolvedAbsolute, "/", isPosixSeparator);
    if (resolvedAbsolute) return `/${resolvedPath}`;
    return resolvedPath.length > 0 ? resolvedPath : ".";
  },
  normalize(path) {
    validateString(path, "path");
    if (path.length === 0) return ".";
    const isAbsolute = path.charCodeAt(0) === SLASH;
    const trailingSeparator = path.charCodeAt(path.length - 1) === SLASH;
    path = normalizeString(path, !isAbsolute, "/", isPosixSeparator);
    if (path.length === 0) {
      if (isAbsolute) return "/";
      return trailingSeparator ? "./" : ".";
    }
    if (trailingSeparator) path += "/";
    return isAbsolute ? `/${path}` : path;
  },
  isAbsolute(path) {
    validateString(path, "path");
    return path.length > 0 && path.charCodeAt(0) === SLASH;
  },
  join(...args) {
    let joined;
    for (const arg of args) {
      validateString(arg, "path");
      if (arg.length > 0) joined = joined === undefined ? arg : `${joined}/${arg}`;
    }
    return joined === undefined ? "." : posix.normalize(joined);
  },
  relative(from, to) {
    validateString(from, "from");
    validateString(to, "to");
    if (from === to) return "";
    from = posix.resolve(from);
    to = posix.resolve(to);
    if (from === to) return "";
    const fromStart = 1;
    const fromEnd = from.length;
    const fromLen = fromEnd - fromStart;
    const toStart = 1;
    const toLen = to.length - toStart;
    const length = fromLen < toLen ? fromLen : toLen;
    let lastCommonSep = -1;
    let i = 0;
    for (; i < length; i++) {
      const fromCode = from.charCodeAt(fromStart + i);
      if (fromCode !== to.charCodeAt(toStart + i)) break;
      else if (fromCode === SLASH) lastCommonSep = i;
    }
    if (i === length) {
      if (toLen > length) {
        if (to.charCodeAt(toStart + i) === SLASH) return to.slice(toStart + i + 1);
        if (i === 0) return to.slice(toStart + i);
      } else if (fromLen > length) {
        if (from.charCodeAt(fromStart + i) === SLASH) lastCommonSep = i;
        else if (i === 0) lastCommonSep = 0;
      }
    }
    let out = "";
    for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
      if (i === fromEnd || from.charCodeAt(i) === SLASH) out += out.length === 0 ? ".." : "/..";
    }
    return `${out}${to.slice(toStart + lastCommonSep)}`;
  },
  toNamespacedPath(path) {
    return path;
  },
  dirname(path) {
    validateString(path, "path");
    if (path.length === 0) return ".";
    const hasRoot = path.charCodeAt(0) === SLASH;
    let end = -1;
    let matchedSlash = true;
    for (let i = path.length - 1; i >= 1; --i) {
      if (path.charCodeAt(i) === SLASH) {
        if (!matchedSlash) {
          end = i;
          break;
        }
      } else {
        matchedSlash = false;
      }
    }
    if (end === -1) return hasRoot ? "/" : ".";
    if (hasRoot && end === 1) return "//";
    return path.slice(0, end);
  },
  basename(path, suffix) {
    validateString(path, "path");
    if (suffix !== undefined) validateString(suffix, "ext");
    return basenameWith(path, suffix, 0, isPosixSeparator);
  },
  extname(path) {
    validateString(path, "path");
    const { end, startDot } = scanSegment(path, 0, isPosixSeparator);
    return startDot === -1 || end === -1 ? "" : path.slice(startDot, end);
  },
  format(pathObject) {
    return format("/", pathObject);
  },
  parse(path) {
    validateString(path, "path");
    const ret = { root: "", dir: "", base: "", ext: "", name: "" };
    if (path.length === 0) return ret;
    const isAbsolute = path.charCodeAt(0) === SLASH;
    if (isAbsolute) ret.root = "/";
    const { startPart, end, startDot } = scanSegment(path, isAbsolute ? 1 : 0, isPosixSeparator);
    if (end !== -1) {
      const start = startPart === 0 && isAbsolute ? 1 : startPart;
      if (startDot === -1) {
        ret.base = ret.name = path.slice(start, end);
      } else {
        ret.name = path.slice(start, startDot);
        ret.base = path.slice(start, end);
        ret.ext = path.slice(startDot, end);
      }
    }
    if (startPart > (isAbsolute ? 1 : 0)) ret.dir = path.slice(0, startPart - 1);
    else if (isAbsolute) ret.dir = "/";
    return ret;
  },
  matchesGlob(path, pattern) {
    return require("internal/glob").matchesGlob(path, pattern, false);
  },
};

const shared = { normalizeString, format, scanSegment, basenameWith, validateString };
let win32;
function loadWin32() {
  if (!win32) {
    win32 = require("internal/path_win32")(shared);
    win32.win32 = win32;
    win32.posix = posix;
  }
  return win32;
}
posix.posix = posix;
Object.defineProperty(posix, "win32", { enumerable: true, get: loadWin32 });
module.exports = internal.config.platform === "win32" ? loadWin32() : posix;
