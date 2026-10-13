// node:querystring, also used by URLSearchParams and the legacy url.parse.
const encoding = native.encoding;
const UNRESERVED = /[A-Za-z0-9\-_.!~*'()]/;
const FORM_SAFE = /[A-Za-z0-9*\-._]/;

function percentEncode(text, safe, spaceAsPlus) {
  let out = "";
  for (const character of String(text)) {
    if (safe.test(character)) out += character;
    else if (spaceAsPlus && character === " ") out += "+";
    else {
      for (const byte of encoding.encode(character, "utf8")) out += "%" + byte.toString(16).toUpperCase().padStart(2, "0");
    }
  }
  return out;
}

function percentDecode(text, plusAsSpace) {
  if (plusAsSpace) text = text.replace(/\+/g, " ");
  if (!text.includes("%")) return text;
  let latin1 = "";
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    const hex = text.slice(i + 1, i + 3);
    if (code === 0x25 && /^[0-9a-fA-F]{2}$/.test(hex)) {
      latin1 += String.fromCharCode(parseInt(hex, 16));
      i += 2;
    } else if (code < 0x80) {
      latin1 += text[i];
    } else {
      latin1 += internal.bytesToLatin1(encoding.encode(text[i], "utf8"));
    }
  }
  return encoding.decode(latin1, "utf8");
}

function escape(text) {
  return percentEncode(text, UNRESERVED, false);
}

function unescape(text) {
  return percentDecode(String(text), false);
}

function stringifyPrimitive(value) {
  if (typeof value === "string") return value;
  if ((typeof value === "number" && Number.isFinite(value)) || typeof value === "bigint" || typeof value === "boolean") return String(value);
  return "";
}

function stringify(object, sep = "&", eq = "=", options = {}) {
  const encode = options.encodeURIComponent || escape;
  if (object === null || typeof object !== "object") return "";
  const pairs = [];
  for (const key of Object.keys(object)) {
    const value = object[key];
    const name = encode(stringifyPrimitive(key)) + eq;
    if (Array.isArray(value)) {
      for (const item of value) pairs.push(name + encode(stringifyPrimitive(item)));
    } else {
      pairs.push(name + encode(stringifyPrimitive(value)));
    }
  }
  return pairs.join(sep);
}

function parse(text, sep = "&", eq = "=", options = {}) {
  const result = Object.create(null);
  if (typeof text !== "string" || text.length === 0) return result;
  const decode = options.decodeURIComponent || unescape;
  const maxKeys = options.maxKeys === undefined ? 1000 : options.maxKeys;
  let parts = text.split(sep);
  if (maxKeys > 0) parts = parts.slice(0, maxKeys);
  for (const part of parts) {
    if (part === "") continue;
    const index = part.indexOf(eq);
    const rawKey = index >= 0 ? part.slice(0, index) : part;
    const rawValue = index >= 0 ? part.slice(index + eq.length) : "";
    const key = decode(rawKey.replace(/\+/g, " "));
    const value = decode(rawValue.replace(/\+/g, " "));
    if (!(key in result)) result[key] = value;
    else if (Array.isArray(result[key])) result[key].push(value);
    else result[key] = [result[key], value];
  }
  return result;
}

module.exports = {
  parse,
  stringify,
  decode: parse,
  encode: stringify,
  escape,
  unescape,
  unescapeBuffer: (text) => require("buffer").Buffer.from(unescape(text)),
};
// application/x-www-form-urlencoded helpers for URLSearchParams.
internal.form = {
  encode: (text) => percentEncode(text, FORM_SAFE, true),
  decode: (text) => percentDecode(text, true),
};
