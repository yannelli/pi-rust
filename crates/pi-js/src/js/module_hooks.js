// module.register hooks and module.SourceMap/findSourceMap. Hooks run on this thread: a hook
// that returns a promise must settle from microtasks alone, since import() waits synchronously.
const errors = require("internal/errors");
const url = require("url");
const path = require("path");
const nativeModules = native.modules;
const hooks = [];
const kDefault = Symbol("defaultLoad");

const settle = (value) => (value && typeof value.then === "function" ? nativeModules.settle(value) : value);
const toKey = (target) => (target.startsWith("file:") ? url.fileURLToPath(target) : target);
const toUrl = (key) => (path.isAbsolute(key) ? url.pathToFileURL(key).href : key);

function invalidReturn(expected, name, property, value) {
  const type = value === null ? "null" : typeof value;
  return errors.codeError(TypeError, "ERR_INVALID_RETURN_PROPERTY_VALUE",
    `Expected ${expected} to be returned for the "${property}" from the "${name}" hook but got ${type}.`);
}

const FORMATS = { ".mjs": "module", ".mts": "module", ".cjs": "commonjs", ".cts": "commonjs", ".json": "json", ".wasm": "wasm" };

function defaultResolve(specifier, context = {}) {
  const parent = context.parentURL && context.parentURL.startsWith("file:") ? url.fileURLToPath(context.parentURL) : null;
  const resolved = nativeModules.resolve(String(specifier), parent, "import");
  if (resolved.kind === "builtin") return { url: resolved.key, format: "builtin" };
  if (resolved.kind !== "file") return { url: resolved.key, format: null };
  return { url: url.pathToFileURL(resolved.key).href, format: FORMATS[path.extname(resolved.key)] ?? null };
}

function defaultLoad(target, context = {}) {
  const key = toKey(target);
  if (key.startsWith("node:")) return { format: "builtin", source: null };
  let source;
  try {
    source = path.isAbsolute(key) ? require("fs").readFileSync(key, "utf8") : undefined;
  } catch {}
  if (source === undefined) return Object.defineProperty({ format: context.format ?? null, source: null }, kDefault, { value: true });
  const format = context.format ?? FORMATS[path.extname(key)] ?? nativeModules.detectFormat(key, source);
  return { format, source };
}

// Chain hooks from the newest registration down to the default, as Node does.
function chain(name, fallback) {
  let next = fallback;
  for (const entry of hooks) {
    const hook = entry[name];
    if (typeof hook !== "function") continue;
    const inner = next;
    next = (first, context) => settle(hook(first, context, inner));
  }
  return next;
}

function hookResolve(specifier, parentPath) {
  const context = { conditions: ["node", "import", "module-sync", "node-addons"], importAttributes: {}, parentURL: parentPath ? toUrl(parentPath) : undefined };
  const result = chain("resolve", defaultResolve)(specifier, context);
  if (!result || typeof result !== "object") throw invalidReturn("an object", "resolve", "url", result);
  if (typeof result.url !== "string") throw invalidReturn("a URL string", "resolve", "url", result.url);
  return toKey(result.url);
}

// undefined lets the native loader read the module itself.
function hookLoad(key) {
  if (!hooks.some((entry) => typeof entry.load === "function")) return undefined;
  const result = chain("load", defaultLoad)(toUrl(key), { conditions: ["node", "import"], format: undefined, importAttributes: {} });
  if (!result || typeof result !== "object") throw invalidReturn("an object", "load", "source", result);
  if (result[kDefault]) return undefined;
  let source = result.source;
  if (source !== null && source !== undefined && typeof source !== "string") {
    if (ArrayBuffer.isView(source) || source instanceof ArrayBuffer) source = new TextDecoder().decode(source);
    else throw invalidReturn("a string, an ArrayBuffer, or a TypedArray", "load", "source", source);
  }
  const format = result.format ?? "module";
  if (source == null && format !== "builtin" && format !== "commonjs") throw invalidReturn("a string, an ArrayBuffer, or a TypedArray", "load", "source", source);
  return { format: String(format), source: source ?? undefined };
}

function register(specifier, parentURL = undefined, options = {}) {
  if (parentURL !== null && typeof parentURL === "object" && !(parentURL instanceof URL)) [options, parentURL] = [parentURL, undefined];
  options = options || {};
  parentURL = parentURL ?? options.parentURL ?? "data:";
  if (specifier instanceof URL) specifier = specifier.href;
  errors.validate.string(specifier, "specifier");
  if (parentURL instanceof URL) parentURL = parentURL.href;
  errors.validate.string(parentURL, "parentURL");
  const parent = parentURL.startsWith("file:") ? url.fileURLToPath(parentURL) : null;
  const target = specifier.startsWith("file:") ? url.fileURLToPath(specifier) : specifier;
  const resolved = nativeModules.resolve(target, parent, "import");
  const namespace = nativeModules.importSync(resolved.key);
  if (typeof namespace.initialize === "function") settle(namespace.initialize(options.data));
  hooks.unshift({ resolve: namespace.resolve, load: namespace.load });
  nativeModules.enableHooks();
}

// Base64 VLQ, as source map mappings use it.
const BASE64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
function decodeMappings(text, sources, names) {
  const mappings = [];
  const state = [0, 0, 0, 0, 0];
  let line = 0;
  for (const group of text.split(";")) {
    state[0] = 0;
    for (const segment of group.split(",")) {
      if (!segment) continue;
      const values = [];
      for (let i = 0, value = 0, shift = 0; i < segment.length; i++) {
        const digit = BASE64.indexOf(segment[i]);
        value += (digit & 31) << shift;
        if (digit & 32) shift += 5;
        else {
          values.push(value & 1 ? -(value >>> 1) : value >>> 1);
          value = shift = 0;
        }
      }
      values.forEach((value, index) => (state[index === 0 ? 0 : index] += value));
      const entry = [line, state[0]];
      if (values.length >= 4) entry.push(sources[state[1]], state[2], state[3]);
      if (values.length >= 5) entry.push(names[state[4]]);
      mappings.push(entry);
    }
    line++;
  }
  return mappings.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
}

function encodeValue(value) {
  let vlq = value < 0 ? (-value << 1) | 1 : value << 1;
  let out = "";
  do {
    let digit = vlq & 31;
    vlq >>>= 5;
    if (vlq > 0) digit |= 32;
    out += BASE64[digit];
  } while (vlq > 0);
  return out;
}

// Mappings text for [generated line, generated column, original line, original column] tokens.
function encodeTokens(tokens) {
  const lines = [];
  let previous = [0, 0, 0];
  for (const [line, column, sourceLine, sourceColumn] of tokens) {
    while (lines.length <= line) {
      lines.push([]);
      previous[0] = 0;
    }
    lines[line].push(encodeValue(column - previous[0]) + "A" + encodeValue(sourceLine - previous[1]) + encodeValue(sourceColumn - previous[2]));
    previous = [column, sourceLine, sourceColumn];
  }
  return lines.map((segments) => segments.join(",")).join(";");
}

class SourceMap {
  #payload;
  #mappings;
  constructor(payload, { lineLengths } = {}) {
    errors.validate.object(payload, "payload");
    this.#payload = structuredClone(payload);
    this.lineLengths = lineLengths;
    const root = payload.sourceRoot || "";
    const sources = (payload.sources || []).map((source) => (source === null ? null : root + source));
    this.#mappings = decodeMappings(String(payload.mappings || ""), sources, payload.names || []);
  }
  get payload() {
    return this.#payload;
  }
  findEntry(lineOffset, columnOffset) {
    let first = 0;
    let count = this.#mappings.length;
    while (count > 1) {
      const step = count >> 1;
      const mapping = this.#mappings[first + step];
      if (lineOffset < mapping[0] || (lineOffset === mapping[0] && columnOffset < mapping[1])) count = step;
      else {
        first += step;
        count -= step;
      }
    }
    const entry = this.#mappings[first];
    if (!entry || (!first && (lineOffset < entry[0] || (lineOffset === entry[0] && columnOffset < entry[1])))) return {};
    const result = { generatedLine: entry[0], generatedColumn: entry[1], originalSource: entry[2], originalLine: entry[3], originalColumn: entry[4] };
    if (entry[5] !== undefined) result.name = entry[5];
    return result;
  }
  findOrigin(lineNumber, columnNumber) {
    const range = this.findEntry(lineNumber - 1, columnNumber - 1);
    if (range.originalSource === undefined || range.originalLine === undefined || range.originalColumn === undefined) return {};
    return {
      name: range.name,
      fileName: range.originalSource,
      lineNumber: range.originalLine + (lineNumber - range.generatedLine),
      columnNumber: range.originalColumn + (columnNumber - range.generatedColumn),
    };
  }
}

function payloadFromComment(file) {
  let text;
  try {
    text = require("fs").readFileSync(file, "utf8");
  } catch {
    return undefined;
  }
  const match = /\/[*/][#@]\s*sourceMappingURL=([^\s*]+)\s*(?:\*\/\s*)?$/.exec(text.trimEnd());
  if (!match) return undefined;
  try {
    const data = /^data:application\/json[^,]*?(;base64)?,(.*)$/.exec(match[1]);
    if (data) return JSON.parse(data[1] ? Buffer.from(data[2], "base64").toString() : decodeURIComponent(data[2]));
    const mapFile = match[1].startsWith("file:") ? url.fileURLToPath(match[1]) : path.resolve(path.dirname(file), match[1]);
    return JSON.parse(require("fs").readFileSync(mapFile, "utf8"));
  } catch {
    return undefined;
  }
}

function findSourceMap(target) {
  if (target instanceof URL || (typeof target === "string" && target.startsWith("file:"))) target = url.fileURLToPath(target);
  errors.validate.string(target, "path");
  const payload = payloadFromComment(target);
  if (payload) return new SourceMap(payload);
  const tokens = nativeModules.sourceMapTokens(target);
  if (!tokens) return undefined;
  return new SourceMap({ version: 3, file: target, sources: [target], names: [], mappings: encodeTokens(tokens) });
}

internal.hookResolve = hookResolve;
internal.hookLoad = hookLoad;
module.exports = { register, findSourceMap, SourceMap };
