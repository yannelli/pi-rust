// Glob matching for path.matchesGlob and fs.glob, with minimatch's rules as Node configures
// them: backslashes are separators, `*` and `**` skip dotfiles, braces and extglobs expand,
// and wildcard segments ignore case on macOS and Windows.
const errors = require("internal/errors");
const platform = internal.config.platform;
const NOCASE = platform === "darwin" || platform === "win32";
const GLOBSTAR = Symbol("globstar");

const escapeRegex = (text) => text.replace(/[-[\]{}()*+?.,\\^$|#\s/]/g, "\\$&");

// Index of the bracket closing the one at `open`, or -1.
function closing(text, open, left, right) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === left) depth++;
    else if (text[i] === right && --depth === 0) return i;
  }
  return -1;
}

function splitTop(text, separator) {
  const parts = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === "{" || c === "(" || c === "[") depth++;
    else if (c === "}" || c === ")" || c === "]") depth--;
    else if (c === separator && depth === 0) {
      parts.push(text.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(text.slice(start));
  return parts;
}

function rangeOf(body) {
  const match = /^(-?\d+|[a-zA-Z])\.\.(-?\d+|[a-zA-Z])(?:\.\.(-?\d+))?$/.exec(body);
  if (!match) return null;
  const numeric = /\d/.test(match[1]);
  const from = numeric ? Number(match[1]) : match[1].charCodeAt(0);
  const to = numeric ? Number(match[2]) : match[2].charCodeAt(0);
  const step = Math.abs(Number(match[3] || 1)) || 1;
  const values = [];
  for (let value = from; from <= to ? value <= to : value >= to; value += from <= to ? step : -step) {
    values.push(numeric ? String(value) : String.fromCharCode(value));
  }
  return values;
}

function expandBraces(pattern) {
  for (let i = 0; i < pattern.length; i++) {
    if (pattern[i] !== "{") continue;
    const close = closing(pattern, i, "{", "}");
    if (close < 0) return [pattern];
    const body = pattern.slice(i + 1, close);
    const options = rangeOf(body) || (splitTop(body, ",").length > 1 ? splitTop(body, ",") : null);
    if (!options) continue;
    const head = pattern.slice(0, i);
    const tail = pattern.slice(close + 1);
    return options.flatMap((option) => expandBraces(head + option + tail));
  }
  return [pattern];
}

const POSIX_CLASSES = {
  alnum: "a-zA-Z0-9", alpha: "a-zA-Z", ascii: "\\x00-\\x7f", blank: " \\t", cntrl: "\\x00-\\x1f\\x7f", digit: "0-9",
  graph: "\\x21-\\x7e", lower: "a-z", print: "\\x20-\\x7e", punct: "!-\\/:-@\\[-`{-~", space: " \\t\\r\\n\\v\\f",
  upper: "A-Z", word: "\\w", xdigit: "A-Fa-f0-9",
};

// A `[...]` class at `start` as [regex source, end index], or null when it never closes.
function bracketClass(segment, start) {
  let i = start + 1;
  let negate = false;
  if (segment[i] === "!" || segment[i] === "^") {
    negate = true;
    i++;
  }
  let body = "";
  for (let first = true; i < segment.length; i++, first = false) {
    const c = segment[i];
    if (c === "]" && !first) return [`[${negate ? "^" : ""}${body}]`, i + 1];
    const posix = c === "[" && /^\[:(\w+):\]/.exec(segment.slice(i));
    if (posix && POSIX_CLASSES[posix[1]]) {
      body += POSIX_CLASSES[posix[1]];
      i += posix[0].length - 1;
    } else body += c === "\\" || c === "]" || c === "[" || c === "^" ? `\\${c}` : c;
  }
  return null;
}

function segmentSource(segment) {
  let source = "";
  let magic = false;
  for (let i = 0; i < segment.length;) {
    const c = segment[i];
    if ("@*+?!".includes(c) && segment[i + 1] === "(") {
      const close = closing(segment, i + 1, "(", ")");
      if (close > 0) {
        const group = `(?:${splitTop(segment.slice(i + 2, close), "|").map((part) => segmentSource(part).source).join("|")})`;
        const rest = c === "!" ? segmentSource(segment.slice(close + 1)).source : "";
        source += { "@": group, "*": `${group}*`, "+": `${group}+`, "?": `${group}?`, "!": `(?:(?!${group}${rest}$)[^/]*?)` }[c];
        magic = true;
        i = close + 1;
        continue;
      }
    }
    if (c === "*") {
      while (segment[i + 1] === "*") i++;
      source += "[^/]*?";
      magic = true;
    } else if (c === "?") {
      source += "[^/]";
      magic = true;
    } else if (c === "[" && bracketClass(segment, i)) {
      const [cls, end] = bracketClass(segment, i);
      source += cls;
      magic = true;
      i = end;
      continue;
    } else source += escapeRegex(c);
    i++;
  }
  return { source, magic };
}

// A segment as a literal string, GLOBSTAR, or a RegExp.
function compileSegment(segment) {
  if (segment === "**") return GLOBSTAR;
  const { source, magic } = segmentSource(segment);
  if (!magic) return segment;
  const dot = segment.startsWith(".") ? "" : "(?!\\.)";
  return new RegExp(`^${dot}${source}$`, NOCASE ? "i" : "");
}

// Every brace alternative of `pattern` as a list of compiled segments.
function compile(pattern) {
  return expandBraces(pattern.replace(/\\/g, "/")).map((alternative) => alternative.split("/").map(compileSegment));
}

const isMagic = (segment) => segment === GLOBSTAR || segment instanceof RegExp;

function matchSegment(segment, part) {
  if (typeof segment === "string") return segment === part;
  return part !== "." && part !== ".." && segment.test(part);
}

function matchSegments(segments, parts, si = 0, pi = 0) {
  for (; si < segments.length; si++, pi++) {
    const segment = segments[si];
    if (segment === GLOBSTAR) {
      for (let k = pi; k <= parts.length; k++) {
        if (matchSegments(segments, parts, si + 1, k)) return true;
        if (k < parts.length && parts[k].startsWith(".")) return false;
      }
      return false;
    }
    if (pi >= parts.length || !matchSegment(segment, parts[pi])) return false;
  }
  return pi === parts.length;
}

function matchesGlob(path, pattern, windows) {
  errors.validate.string(path, "path");
  errors.validate.string(pattern, "pattern");
  const parts = (windows ? path.replace(/\\/g, "/") : path).split("/");
  return compile(pattern).some((segments) => matchSegments(segments, parts));
}

// fs.glob: walk from `cwd`, following the compiled segments.
function globSync(pattern, options = {}) {
  const fs = require("fs");
  const path = require("path");
  const patterns = typeof pattern === "string" ? [pattern] : pattern;
  if (!Array.isArray(patterns) || patterns.some((item) => typeof item !== "string")) {
    throw errors.invalidArgType("pattern", "of type string or an instance of Array", pattern);
  }
  if (options === null || typeof options !== "object") throw errors.invalidArgType("options", "of type object", options);
  const cwd = options.cwd === undefined ? process.cwd() : require("internal/fs_utils").toPath(options.cwd, "options.cwd");
  const { exclude, withFileTypes } = options;
  if (exclude !== undefined && typeof exclude !== "function" && !Array.isArray(exclude)) {
    throw errors.invalidArgType("options.exclude", "of type function or an instance of Array", exclude);
  }
  const excludes = Array.isArray(exclude) ? exclude.flatMap((item) => compile(item)) : [];
  const results = new Map();
  const seen = new Set();
  const direntOf = (full) => {
    const stats = fs.lstatSync(full);
    const type = stats.isSymbolicLink() ? 3 : stats.isDirectory() ? 2 : stats.isFile() ? 1 : 0;
    return new fs.Dirent(path.basename(full), type, path.dirname(full));
  };
  const excluded = (relative, full) => {
    if (typeof exclude === "function") return !!exclude(withFileTypes ? direntOf(full) : relative);
    const parts = relative.split(path.sep);
    return excludes.some((segments) => matchSegments(segments, parts));
  };
  const add = (relative, full) => {
    if (!results.has(relative) && !excluded(relative, full)) results.set(relative, full);
  };
  const entries = (full) => {
    try {
      return fs.readdirSync(full, { withFileTypes: true });
    } catch {
      return [];
    }
  };
  const isDir = (full) => {
    try {
      return fs.statSync(full).isDirectory();
    } catch {
      return false;
    }
  };
  const walk = (segments, index, relative, full) => {
    const key = `${index}\u0000${full}`;
    if (seen.has(key)) return;
    seen.add(key);
    if (index === segments.length) return add(relative || ".", full);
    if (relative && excluded(relative, full)) return;
    const segment = segments[index];
    const join = (name) => [relative ? path.join(relative, name) : name, path.join(full, name)];
    if (segment === GLOBSTAR) {
      walk(segments, index + 1, relative, full);
      for (const entry of entries(full)) {
        if (entry.name.startsWith(".") || !entry.isDirectory()) continue;
        walk(segments, index, ...join(entry.name));
      }
      return;
    }
    if (typeof segment === "string") {
      const [nextRelative, nextFull] = join(segment);
      const last = index === segments.length - 1;
      if (segment === "" && last) return;
      if (last ? fs.existsSync(nextFull) || isLink(fs, nextFull) : isDir(nextFull)) walk(segments, index + 1, nextRelative, nextFull);
      return;
    }
    for (const entry of entries(full)) {
      if (!matchSegment(segment, entry.name)) continue;
      const [nextRelative, nextFull] = join(entry.name);
      if (index === segments.length - 1 || entry.isDirectory() || (entry.isSymbolicLink() && isDir(nextFull))) {
        walk(segments, index + 1, nextRelative, nextFull);
      }
    }
  };
  for (const item of patterns) {
    for (const segments of compile(item)) {
      seen.clear();
      if (path.isAbsolute(item)) {
        const root = segments[0] === "" ? path.parse(cwd).root : `${segments[0]}${path.sep}`;
        walkAbsolute(segments, root, walk);
      } else walk(segments, 0, "", cwd);
    }
  }
  return [...results].map(([relative, full]) => (withFileTypes ? direntOf(full) : relative));
}

function isLink(fs, full) {
  try {
    return fs.lstatSync(full).isSymbolicLink();
  } catch {
    return false;
  }
}

// Absolute patterns report absolute paths: walk from the root with the root as the prefix.
function walkAbsolute(segments, root, walk) {
  walk(segments, 1, root, root);
}

module.exports = { matchesGlob, globSync, compile, expandBraces, GLOBSTAR, isMagic };
