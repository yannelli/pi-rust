// path.win32, ported from Node's lib/path.js. Receives helpers shared with the posix flavor.
module.exports = function createWin32({ normalizeString, format, scanSegment, basenameWith, validateString }) {
  const SLASH = 47;
  const BACKSLASH = 92;
  const COLON = 58;
  const QUESTION = 63;
  const DOT = 46;
  const isSep = (code) => code === SLASH || code === BACKSLASH;
  const isDeviceRoot = (code) => (code >= 65 && code <= 90) || (code >= 97 && code <= 122);
  const hasDrive = (path) => path.length >= 2 && isDeviceRoot(path.charCodeAt(0)) && path.charCodeAt(1) === COLON;

  // UNC root `\\server\share`: returns { end, device, complete } or null.
  function uncRoot(path) {
    const len = path.length;
    let j = 2;
    let last = j;
    while (j < len && !isSep(path.charCodeAt(j))) j++;
    if (j >= len || j === last) return null;
    const firstPart = path.slice(last, j);
    last = j;
    while (j < len && isSep(path.charCodeAt(j))) j++;
    if (j >= len || j === last) return null;
    last = j;
    while (j < len && !isSep(path.charCodeAt(j))) j++;
    if (j !== len && j === last) return null;
    return { end: j, device: `\\\\${firstPart}\\${path.slice(last, j)}`, complete: j === len };
  }

  // Root of a path: device ("C:" or UNC), where the root ends, and whether it is absolute.
  function root(path) {
    const len = path.length;
    const code = path.charCodeAt(0);
    if (len === 1) return { device: "", rootEnd: isSep(code) ? 1 : 0, isAbsolute: isSep(code) };
    if (isSep(code)) {
      if (isSep(path.charCodeAt(1))) {
        const unc = uncRoot(path);
        if (unc) return { device: unc.device, rootEnd: unc.end, isAbsolute: true, unc };
        return { device: "", rootEnd: 0, isAbsolute: true };
      }
      return { device: "", rootEnd: 1, isAbsolute: true };
    }
    if (hasDrive(path)) {
      const absolute = len > 2 && isSep(path.charCodeAt(2));
      return { device: path.slice(0, 2), rootEnd: absolute ? 3 : 2, isAbsolute: absolute };
    }
    return { device: "", rootEnd: 0, isAbsolute: false };
  }

  const win32 = {
    sep: "\\",
    delimiter: ";",
    resolve(...args) {
      let resolvedDevice = "";
      let resolvedTail = "";
      let resolvedAbsolute = false;
      for (let i = args.length - 1; i >= -1; i--) {
        let path;
        if (i >= 0) {
          path = args[i];
          validateString(path, `paths[${i}]`);
          if (path.length === 0) continue;
        } else if (resolvedDevice.length === 0) {
          path = native.process.cwd();
        } else {
          path = (globalThis.process && process.env[`=${resolvedDevice}`]) || native.process.cwd();
          if (path.slice(0, 2).toLowerCase() !== resolvedDevice.toLowerCase() && path.charCodeAt(2) === BACKSLASH) {
            path = `${resolvedDevice}\\`;
          }
        }
        const { device, rootEnd, isAbsolute } = root(path);
        if (device.length > 0) {
          if (resolvedDevice.length > 0) {
            if (device.toLowerCase() !== resolvedDevice.toLowerCase()) continue;
          } else {
            resolvedDevice = device;
          }
        }
        if (resolvedAbsolute) {
          if (resolvedDevice.length > 0) break;
        } else {
          resolvedTail = `${path.slice(rootEnd)}\\${resolvedTail}`;
          resolvedAbsolute = isAbsolute;
          if (isAbsolute && resolvedDevice.length > 0) break;
        }
      }
      resolvedTail = normalizeString(resolvedTail, !resolvedAbsolute, "\\", isSep);
      return resolvedAbsolute ? `${resolvedDevice}\\${resolvedTail}` : `${resolvedDevice}${resolvedTail}` || ".";
    },
    normalize(path) {
      validateString(path, "path");
      const len = path.length;
      if (len === 0) return ".";
      if (len === 1) return path.charCodeAt(0) === SLASH ? "\\" : path;
      const parsed = root(path);
      if (parsed.unc && parsed.unc.complete) return `${parsed.device}\\`;
      let tail = parsed.rootEnd < len ? normalizeString(path.slice(parsed.rootEnd), !parsed.isAbsolute, "\\", isSep) : "";
      if (tail.length === 0 && !parsed.isAbsolute) tail = ".";
      if (tail.length > 0 && isSep(path.charCodeAt(len - 1))) tail += "\\";
      if (!parsed.device) return parsed.isAbsolute ? `\\${tail}` : tail;
      return parsed.isAbsolute ? `${parsed.device}\\${tail}` : `${parsed.device}${tail}`;
    },
    isAbsolute(path) {
      validateString(path, "path");
      const len = path.length;
      if (len === 0) return false;
      const code = path.charCodeAt(0);
      return isSep(code) || (len > 2 && hasDrive(path) && isSep(path.charCodeAt(2)));
    },
    join(...args) {
      let joined;
      let firstPart;
      for (const arg of args) {
        validateString(arg, "path");
        if (arg.length > 0) {
          if (joined === undefined) joined = firstPart = arg;
          else joined += `\\${arg}`;
        }
      }
      if (joined === undefined) return ".";
      let needsReplace = true;
      let slashCount = 0;
      if (isSep(firstPart.charCodeAt(0))) {
        ++slashCount;
        const firstLen = firstPart.length;
        if (firstLen > 1 && isSep(firstPart.charCodeAt(1))) {
          ++slashCount;
          if (firstLen > 2) {
            if (isSep(firstPart.charCodeAt(2))) ++slashCount;
            else needsReplace = false;
          }
        }
      }
      if (needsReplace) {
        while (slashCount < joined.length && isSep(joined.charCodeAt(slashCount))) slashCount++;
        if (slashCount >= 2) joined = `\\${joined.slice(slashCount)}`;
      }
      return win32.normalize(joined);
    },
    relative(from, to) {
      validateString(from, "from");
      validateString(to, "to");
      if (from === to) return "";
      const fromOrig = win32.resolve(from);
      const toOrig = win32.resolve(to);
      if (fromOrig === toOrig) return "";
      from = fromOrig.toLowerCase();
      to = toOrig.toLowerCase();
      if (from === to) return "";
      let fromStart = 0;
      while (fromStart < from.length && from.charCodeAt(fromStart) === BACKSLASH) fromStart++;
      let fromEnd = from.length;
      while (fromEnd - 1 > fromStart && from.charCodeAt(fromEnd - 1) === BACKSLASH) fromEnd--;
      const fromLen = fromEnd - fromStart;
      let toStart = 0;
      while (toStart < to.length && to.charCodeAt(toStart) === BACKSLASH) toStart++;
      let toEnd = to.length;
      while (toEnd - 1 > toStart && to.charCodeAt(toEnd - 1) === BACKSLASH) toEnd--;
      const toLen = toEnd - toStart;
      const length = fromLen < toLen ? fromLen : toLen;
      let lastCommonSep = -1;
      let i = 0;
      for (; i < length; i++) {
        const fromCode = from.charCodeAt(fromStart + i);
        if (fromCode !== to.charCodeAt(toStart + i)) break;
        else if (fromCode === BACKSLASH) lastCommonSep = i;
      }
      if (i !== length) {
        if (lastCommonSep === -1) return toOrig;
      } else {
        if (toLen > length) {
          if (to.charCodeAt(toStart + i) === BACKSLASH) return toOrig.slice(toStart + i + 1);
          if (i === 2) return toOrig.slice(toStart + i);
        }
        if (fromLen > length) {
          if (from.charCodeAt(fromStart + i) === BACKSLASH) lastCommonSep = i;
          else if (i === 2) lastCommonSep = 3;
        }
        if (lastCommonSep === -1) lastCommonSep = 0;
      }
      let out = "";
      for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
        if (i === fromEnd || from.charCodeAt(i) === BACKSLASH) out += out.length === 0 ? ".." : "\\..";
      }
      toStart += lastCommonSep;
      if (out.length > 0) return `${out}${toOrig.slice(toStart, toEnd)}`;
      if (toOrig.charCodeAt(toStart) === BACKSLASH) ++toStart;
      return toOrig.slice(toStart, toEnd);
    },
    toNamespacedPath(path) {
      if (typeof path !== "string" || path.length === 0) return path;
      const resolvedPath = win32.resolve(path);
      if (resolvedPath.length <= 2) return path;
      if (resolvedPath.charCodeAt(0) === BACKSLASH) {
        if (resolvedPath.charCodeAt(1) === BACKSLASH) {
          const code = resolvedPath.charCodeAt(2);
          if (code !== QUESTION && code !== DOT) return `\\\\?\\UNC\\${resolvedPath.slice(2)}`;
        }
      } else if (hasDrive(resolvedPath) && resolvedPath.charCodeAt(2) === BACKSLASH) {
        return `\\\\?\\${resolvedPath}`;
      }
      return path;
    },
    dirname(path) {
      validateString(path, "path");
      const len = path.length;
      if (len === 0) return ".";
      if (len === 1) return isSep(path.charCodeAt(0)) ? path : ".";
      let rootEnd = -1;
      let offset = 0;
      const parsed = root(path);
      if (parsed.unc) {
        if (parsed.unc.complete) return path;
        rootEnd = offset = parsed.unc.end + 1;
      } else if (isSep(path.charCodeAt(0))) {
        rootEnd = offset = 1;
      } else if (parsed.device) {
        rootEnd = offset = parsed.rootEnd;
      }
      let end = -1;
      let matchedSlash = true;
      for (let i = len - 1; i >= offset; --i) {
        if (isSep(path.charCodeAt(i))) {
          if (!matchedSlash) {
            end = i;
            break;
          }
        } else {
          matchedSlash = false;
        }
      }
      if (end === -1) {
        if (rootEnd === -1) return ".";
        end = rootEnd;
      }
      return path.slice(0, end);
    },
    basename(path, suffix) {
      validateString(path, "path");
      if (suffix !== undefined) validateString(suffix, "ext");
      return basenameWith(path, suffix, hasDrive(path) ? 2 : 0, isSep);
    },
    extname(path) {
      validateString(path, "path");
      const { end, startDot } = scanSegment(path, hasDrive(path) ? 2 : 0, isSep);
      return startDot === -1 || end === -1 ? "" : path.slice(startDot, end);
    },
    format(pathObject) {
      return format("\\", pathObject);
    },
    parse(path) {
      validateString(path, "path");
      const ret = { root: "", dir: "", base: "", ext: "", name: "" };
      const len = path.length;
      if (len === 0) return ret;
      let rootEnd = 0;
      if (len === 1) {
        if (isSep(path.charCodeAt(0))) ret.root = ret.dir = path;
        else ret.base = ret.name = path;
        return ret;
      }
      const parsed = root(path);
      if (parsed.unc) {
        rootEnd = parsed.unc.complete ? parsed.unc.end : parsed.unc.end + 1;
      } else if (isSep(path.charCodeAt(0))) {
        rootEnd = 1;
      } else if (parsed.device) {
        if (len <= 2 || (len === 3 && parsed.isAbsolute)) {
          ret.root = ret.dir = path;
          return ret;
        }
        rootEnd = parsed.rootEnd;
      }
      if (rootEnd > 0) ret.root = path.slice(0, rootEnd);
      const { startPart, end, startDot } = scanSegment(path, rootEnd, isSep);
      if (end !== -1) {
        if (startDot === -1) {
          ret.base = ret.name = path.slice(startPart, end);
        } else {
          ret.name = path.slice(startPart, startDot);
          ret.base = path.slice(startPart, end);
          ret.ext = path.slice(startDot, end);
        }
      }
      ret.dir = startPart > 0 && startPart !== rootEnd ? path.slice(0, startPart - 1) : ret.root;
      return ret;
    },
    matchesGlob() {
      throw internal.unsupported("path.matchesGlob");
    },
  };
  return win32;
};
