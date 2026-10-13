// util.format and the value formatter behind console output, following Node's layout.
const customInspect = Symbol.for("nodejs.util.inspect.custom");
const defaultOptions = {
  showHidden: false,
  depth: 2,
  colors: false,
  customInspect: true,
  maxArrayLength: 100,
  maxStringLength: 10000,
  breakLength: 80,
  compact: 3,
  sorted: false,
  getters: false,
};
const colors = {
  bold: [1, 22], italic: [3, 23], underline: [4, 24], inverse: [7, 27], white: [37, 39], grey: [90, 39],
  black: [30, 39], blue: [34, 39], cyan: [36, 39], green: [32, 39], magenta: [35, 39], red: [31, 39], yellow: [33, 39],
};
colors.gray = colors.grey;
const styles = {
  special: "cyan", number: "yellow", bigint: "yellow", boolean: "yellow", undefined: "grey", null: "bold",
  string: "green", symbol: "green", date: "magenta", regexp: "red", module: "underline",
};

function stylize(ctx, text, type) {
  if (!ctx.colors) return text;
  const color = colors[styles[type]];
  return color ? `\u001b[${color[0]}m${text}\u001b[${color[1]}m` : text;
}

const IDENTIFIER = /^[A-Za-z_$][\w$]*$/;
const INDEX = /^(0|[1-9]\d*)$/;
const ESCAPES = { "\n": "\\n", "\t": "\\t", "\r": "\\r", "\b": "\\b", "\f": "\\f", "\v": "\\v", "\\": "\\\\" };

function pickQuote(text) {
  if (!text.includes("'")) return "'";
  if (!text.includes('"')) return '"';
  if (!text.includes("`") && !text.includes("${")) return "`";
  return "'";
}

function quote(text) {
  const quoteChar = pickQuote(text);
  let out = "";
  for (const char of text) {
    if (char === quoteChar) out += "\\" + char;
    else if (ESCAPES[char]) out += ESCAPES[char];
    else if (char < " " || char === "\x7f") out += "\\x" + char.charCodeAt(0).toString(16).padStart(2, "0");
    else out += char;
  }
  return quoteChar + out + quoteChar;
}

function plural(count, word) {
  return `${count} ${word}${count > 1 ? "s" : ""}`;
}

function formatPrimitive(ctx, value) {
  switch (typeof value) {
    case "string": {
      if (value.length <= ctx.maxStringLength) return stylize(ctx, quote(value), "string");
      const rest = value.length - ctx.maxStringLength;
      return stylize(ctx, quote(value.slice(0, ctx.maxStringLength)), "string") + `... ${plural(rest, "more character")}`;
    }
    case "number":
      return stylize(ctx, Object.is(value, -0) ? "-0" : String(value), "number");
    case "bigint":
      return stylize(ctx, `${value}n`, "bigint");
    case "boolean":
      return stylize(ctx, String(value), "boolean");
    case "undefined":
      return stylize(ctx, "undefined", "undefined");
    case "symbol":
      return stylize(ctx, value.toString(), "symbol");
    default:
      return stylize(ctx, "null", "null");
  }
}

function constructorName(value) {
  let proto = value;
  while (proto) {
    const descriptor = Object.getOwnPropertyDescriptor(proto, "constructor");
    if (descriptor && typeof descriptor.value === "function" && descriptor.value.name) return descriptor.value.name;
    proto = Object.getPrototypeOf(proto);
  }
  return null;
}

function formatKey(ctx, key) {
  if (typeof key === "symbol") return `[${stylize(ctx, key.toString(), "symbol")}]`;
  return IDENTIFIER.test(key) ? key : stylize(ctx, quote(key), "string");
}

const FUNCTION_TYPES = { AsyncFunction: 1, GeneratorFunction: 1, AsyncGeneratorFunction: 1 };

function functionBase(value) {
  const source = Function.prototype.toString.call(value);
  const name = value.name;
  if (/^class\b/.test(source)) {
    const parent = Object.getPrototypeOf(value);
    const extendsPart = parent && parent.name ? ` extends ${parent.name}` : "";
    return `[class ${name || "(anonymous)"}${extendsPart}]`;
  }
  const tag = value[Symbol.toStringTag];
  const type = FUNCTION_TYPES[tag] ? tag : "Function";
  return name ? `[${type}: ${name}]` : `[${type} (anonymous)]`;
}

function formatError(value) {
  const name = value.name || "Error";
  const message = value.message === undefined ? "" : String(value.message);
  const head = message ? `${name}: ${message}` : name;
  const stack = typeof value.stack === "string" ? value.stack : "";
  if (!stack) return `[${head}]`;
  return stack.startsWith(head) ? stack.trimEnd() : `${head}\n${stack.trimEnd()}`;
}

function fitsOnOneLine(ctx, output, start, base) {
  let total = output.length + start;
  for (const entry of output) total += entry.length;
  return total <= ctx.breakLength && !base.includes("\n");
}

function reduceToSingleString(ctx, output, base, braces, level) {
  const head = base ? `${base} ` : "";
  if (output.length === 0) return `${head}${braces[0]}${braces[1]}`;
  if (ctx.compact === true) return `${head}${braces[0]} ${output.join(", ")} ${braces[1]}`;
  const start = output.length + ctx.indentationLvl + braces[0].length + base.length + 10;
  if (ctx.compact >= 1 && level <= ctx.compact && fitsOnOneLine(ctx, output, start, base)) {
    const joined = output.join(", ");
    if (!joined.includes("\n")) return `${head}${braces[0]} ${joined} ${braces[1]}`;
  }
  const indentation = `\n${" ".repeat(ctx.indentationLvl)}`;
  return `${head}${braces[0]}${indentation}  ${output.join(`,${indentation}  `)}${indentation}${braces[1]}`;
}

function nested(ctx, format) {
  ctx.indentationLvl += 2;
  try {
    return format();
  } finally {
    ctx.indentationLvl -= 2;
  }
}

function formatProperty(ctx, value, key, recurseTimes) {
  const descriptor = Object.getOwnPropertyDescriptor(value, key) || { value: value[key], enumerable: true };
  let formatted;
  if (descriptor.value !== undefined || !("get" in descriptor)) {
    formatted = nested(ctx, () => formatValue(ctx, descriptor.value, recurseTimes));
  } else if (descriptor.get !== undefined) {
    formatted = stylize(ctx, descriptor.set !== undefined ? "[Getter/Setter]" : "[Getter]", "special");
  } else if (descriptor.set !== undefined) {
    formatted = stylize(ctx, "[Setter]", "special");
  } else {
    formatted = stylize(ctx, "undefined", "undefined");
  }
  const name = descriptor.enumerable === false ? `[${formatKey(ctx, key)}]` : formatKey(ctx, key);
  return `${name}: ${formatted}`;
}

function ownKeys(ctx, value) {
  const names = ctx.showHidden ? Object.getOwnPropertyNames(value) : Object.keys(value);
  const symbols = Object.getOwnPropertySymbols(value).filter((symbol) =>
    ctx.showHidden || Object.prototype.propertyIsEnumerable.call(value, symbol));
  const keys = names.concat(symbols);
  if (ctx.sorted) keys.sort(typeof ctx.sorted === "function" ? ctx.sorted : undefined);
  return keys;
}

function formatValue(ctx, value, recurseTimes) {
  if (value === null || (typeof value !== "object" && typeof value !== "function")) return formatPrimitive(ctx, value);
  if (ctx.customInspect) {
    const custom = value[customInspect];
    if (typeof custom === "function" && custom !== inspect) {
      const depth = ctx.depth === null ? null : ctx.depth - recurseTimes;
      const options = Object.assign({}, ctx, { stylize: (text, type) => stylize(ctx, text, type) });
      const result = custom.call(value, depth, options, inspect);
      if (result !== value) return typeof result === "string" ? result : formatValue(ctx, result, recurseTimes);
    }
  }
  if (ctx.seen.includes(value)) {
    let index = ctx.circular.get(value);
    if (index === undefined) {
      index = ctx.circular.size + 1;
      ctx.circular.set(value, index);
    }
    return stylize(ctx, `[Circular *${index}]`, "special");
  }
  return formatRaw(ctx, value, recurseTimes);
}

function isBoxed(value) {
  return value instanceof Number || value instanceof String || value instanceof Boolean ||
    value instanceof BigInt || value instanceof Symbol;
}

// Describe how one object prints: its base text, braces, entries, and keys to show.
function describe(ctx, value, name, prefix, keys) {
  const withoutIndexes = () => keys.filter((key) => !(typeof key === "string" && INDEX.test(key)));
  if (Array.isArray(value)) {
    const shown = withoutIndexes().filter((key) => ctx.showHidden || key !== "length");
    return { braces: [`${name === "Array" ? "" : prefix}[`, "]"], keys: shown, entries: (level) => formatList(ctx, value, level) };
  }
  if (typeof value === "function") {
    const hidden = ["length", "name", "prototype", "arguments", "caller"];
    return { base: functionBase(value), keys: keys.filter((key) => !hidden.includes(key)), alone: true, type: "special" };
  }
  if (value instanceof Map) {
    const entries = (level) => Array.from(value, ([k, v]) => `${formatValue(ctx, k, level)} => ${formatValue(ctx, v, level)}`);
    return { braces: [`${name || "Map"}(${value.size}) {`, "}"], keys, entries };
  }
  if (value instanceof Set) {
    return { braces: [`${name || "Set"}(${value.size}) {`, "}"], keys, entries: (level) => Array.from(value, (item) => formatValue(ctx, item, level)) };
  }
  if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
    return { braces: [`${name}(${value.length}) [`, "]"], keys: withoutIndexes(), entries: (level) => formatList(ctx, value, level) };
  }
  if (value instanceof Date) {
    const text = Number.isNaN(value.getTime()) ? "Invalid Date" : value.toISOString();
    return { base: stylize(ctx, text, "date"), keys, alone: true };
  }
  if (value instanceof RegExp) {
    const text = RegExp.prototype.toString.call(value);
    return { base: stylize(ctx, text, "regexp"), keys: keys.filter((key) => key !== "lastIndex"), alone: true };
  }
  if (value instanceof Error) {
    return { base: formatError(value), keys: keys.filter((key) => key !== "stack" && key !== "message"), alone: true };
  }
  if (value instanceof Promise) {
    const state = native.util.promiseState(value);
    const entries = (level) => {
      if (state.state === "pending") return ["<pending>"];
      const shown = formatValue(ctx, state.value, level);
      return [state.state === "rejected" ? `<rejected> ${shown}` : shown];
    };
    return { braces: ["Promise {", "}"], keys, entries };
  }
  if (isBoxed(value)) {
    const primitive = value.valueOf();
    const base = `[${name}: ${formatPrimitive(Object.assign({}, ctx, { colors: false }), primitive)}]`;
    const shown = value instanceof String ? withoutIndexes().filter((key) => key !== "length") : keys;
    return { base: stylize(ctx, base, typeof primitive), keys: shown, alone: true };
  }
  return { braces: [`${prefix}{`, "}"], keys };
}

function formatRaw(ctx, value, recurseTimes) {
  const name = constructorName(value);
  if (value instanceof ArrayBuffer) {
    const bytes = Array.from(new Uint8Array(value).slice(0, 50), (byte) => byte.toString(16).padStart(2, "0"));
    return `ArrayBuffer { [Uint8Contents]: <${bytes.join(" ")}${value.byteLength > 50 ? " ..." : ""}>, byteLength: ${value.byteLength} }`;
  }
  if (value instanceof WeakMap || value instanceof WeakSet) return `${name} { <items unknown> }`;
  const tag = value[Symbol.toStringTag];
  let prefix = name === null ? "[Object: null prototype] " : name !== "Object" ? `${name} ` : "";
  if (typeof tag === "string" && tag !== "" && tag !== name) prefix = `${prefix}[${tag}] `;
  const shape = describe(ctx, value, name, prefix, ownKeys(ctx, value));
  if (shape.alone && shape.keys.length === 0) return shape.base;
  if (!shape.entries && shape.keys.length === 0 && !shape.base) return `${shape.braces[0]}}`;
  if (ctx.depth !== null && recurseTimes > ctx.depth) return stylize(ctx, `[${name || tag || "Object"}]`, "special");
  ctx.seen.push(value);
  const output = shape.entries ? shape.entries(recurseTimes + 1) : [];
  for (const key of shape.keys) output.push(formatProperty(ctx, value, key, recurseTimes + 1));
  ctx.seen.pop();
  let result = reduceToSingleString(ctx, output, shape.base || "", shape.braces || ["{", "}"], recurseTimes);
  const index = ctx.circular.get(value);
  if (index !== undefined) result = `${stylize(ctx, `<ref *${index}>`, "special")} ${result}`;
  return result;
}

function formatList(ctx, value, recurseTimes) {
  const output = [];
  const limit = Math.min(ctx.maxArrayLength, value.length);
  let holes = 0;
  const flushHoles = () => {
    if (holes) output.push(stylize(ctx, `<${plural(holes, "empty item")}>`, "undefined"));
    holes = 0;
  };
  for (let i = 0; i < limit; i++) {
    if (!Object.prototype.hasOwnProperty.call(value, i)) {
      holes++;
      continue;
    }
    flushHoles();
    output.push(nested(ctx, () => formatValue(ctx, value[i], recurseTimes)));
  }
  flushHoles();
  if (value.length > limit) output.push(`... ${plural(value.length - limit, "more item")}`);
  return output;
}

function inspect(value, options) {
  const ctx = Object.assign({}, defaultOptions, inspect.defaultOptions, { seen: [], circular: new Map(), indentationLvl: 0 });
  if (typeof options === "boolean") ctx.showHidden = options;
  else if (options) Object.assign(ctx, options);
  if (arguments.length > 2 && arguments[2] !== undefined) ctx.depth = arguments[2];
  if (arguments.length > 3 && arguments[3] !== undefined) ctx.colors = arguments[3];
  if (ctx.depth === Infinity) ctx.depth = null;
  if (ctx.maxArrayLength === null) ctx.maxArrayLength = Infinity;
  if (ctx.maxStringLength === null) ctx.maxStringLength = Infinity;
  return formatValue(ctx, value, 0);
}
inspect.custom = customInspect;
inspect.colors = colors;
inspect.styles = styles;
inspect.defaultOptions = {};

function formatArgument(code, arg, options) {
  switch (code) {
    case "s":
      if (typeof arg === "bigint") return `${arg}n`;
      if (typeof arg === "symbol") return arg.toString();
      if (arg !== null && typeof arg === "object") return inspect(arg, Object.assign({}, options, { depth: 0 }));
      return String(arg);
    case "d":
      if (typeof arg === "bigint") return `${arg}n`;
      return typeof arg === "symbol" ? "NaN" : formatPrimitive({ colors: false }, Number(arg));
    case "i":
      if (typeof arg === "bigint") return `${arg}n`;
      return typeof arg === "symbol" ? "NaN" : String(parseInt(arg, 10));
    case "f":
      return typeof arg === "symbol" ? "NaN" : String(parseFloat(arg));
    case "j":
      try {
        return JSON.stringify(arg);
      } catch {
        return "[Circular]";
      }
    case "o":
      return inspect(arg, Object.assign({}, options, { showHidden: true, depth: 4 }));
    case "O":
      return inspect(arg, options);
    default:
      return "";
  }
}

function formatWithOptions(options, ...args) {
  const first = args[0];
  let start = 0;
  let out = "";
  if (typeof first === "string") {
    if (args.length === 1) return first;
    let index = 1;
    let last = 0;
    for (let i = 0; i < first.length - 1; i++) {
      if (first.charCodeAt(i) !== 37) continue;
      const code = first[i + 1];
      if (code === "%") {
        out += first.slice(last, i) + "%";
        last = i + 2;
        i++;
      } else if (index < args.length && "sdifjoOc".includes(code)) {
        out += first.slice(last, i) + formatArgument(code, args[index], options);
        last = i + 2;
        i++;
        index++;
      }
    }
    out += first.slice(last);
    start = index;
  }
  for (let i = start; i < args.length; i++) {
    const arg = args[i];
    if (i > 0) out += " ";
    out += typeof arg === "string" ? arg : inspect(arg, options);
  }
  return out;
}

function format(...args) {
  return formatWithOptions(undefined, ...args);
}

module.exports = { inspect, format, formatWithOptions, stylize };
