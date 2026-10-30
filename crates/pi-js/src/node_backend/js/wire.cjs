"use strict";
// Values between Rust and JavaScript: ["u"] undefined, ["j", json], ["b", base64], ["r", refId].
// Encoders return JSON text so values go through JSON.stringify once.

const refs = new Map();
let nextRef = 1;

Object.defineProperty(globalThis, Symbol.for("pi.node.refCount"), { value: () => refs.size });

function keep(value) {
  const id = nextRef++;
  refs.set(id, value);
  return id;
}

function release(ids) {
  for (const id of ids) refs.delete(id);
}

function lookup(id) {
  if (!refs.has(id)) throw new Error("JsRef was released");
  return refs.get(id);
}

function bytesOf(value) {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  return undefined;
}

function encode(value) {
  if (value === undefined) return '["u"]';
  if (typeof value === "function") return `["r",${keep(value)}]`;
  const bytes = bytesOf(value);
  if (bytes) return `["b","${Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString("base64")}"]`;
  const text = JSON.stringify(value);
  return text === undefined ? '["u"]' : `["j",${text}]`;
}

function encodeAs(value, want) {
  switch (want) {
    case "ref":
      return `["r",${keep(value)}]`;
    case "optRef":
      return value === undefined || value === null ? '["u"]' : `["r",${keep(value)}]`;
    case "json":
      return `["j",${JSON.stringify(value) ?? "null"}]`;
    case "unit":
      return '["u"]';
    default:
      return encode(value);
  }
}

function decode(wire) {
  switch (wire[0]) {
    case "j":
      return wire[1];
    case "b":
      return Uint8Array.from(Buffer.from(wire[1], "base64"));
    case "r":
      return lookup(wire[1]);
    default:
      return undefined;
  }
}

const CONSTRUCTORS = { TypeError, RangeError, SyntaxError, ReferenceError, EvalError, URIError };

function toError(error) {
  const Constructor = CONSTRUCTORS[error.name] ?? Error;
  const result = new Constructor(error.message);
  if (error.name && error.name !== Constructor.name) result.name = error.name;
  if (error.code) result.code = error.code;
  if (error.stack) result.stack = `${result.name}: ${result.message}\n${error.stack}`;
  Object.assign(result, error.data ?? {});
  return result;
}

function frames(stack) {
  const lines = String(stack).split("\n").filter((line) => /^\s+at /.test(line));
  return lines.length ? lines.join("\n") : undefined;
}

function errorJson(error) {
  if (error === null || (typeof error !== "object" && typeof error !== "function") || typeof error === "function") {
    return JSON.stringify({ name: "", message: String(error) });
  }
  const result = { name: String(error.name ?? "Error"), message: String(error.message ?? "") };
  const stack = typeof error.stack === "string" ? frames(error.stack) : undefined;
  if (stack) result.stack = stack;
  if (error.code !== undefined) result.code = String(error.code);
  try {
    const data = JSON.parse(JSON.stringify(error) ?? "{}");
    for (const key of ["name", "message", "stack", "code"]) delete data[key];
    if (data && typeof data === "object" && Object.keys(data).length) result.data = data;
  } catch {}
  return JSON.stringify(result);
}

module.exports = { keep, release, lookup, encode, encodeAs, decode, toError, errorJson };
