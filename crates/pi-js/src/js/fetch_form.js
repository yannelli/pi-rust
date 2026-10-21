// internal/fetch_form: Body.formData parsing for multipart/form-data and urlencoded bodies,
// following undici's formdata-parser.js (the parser Node 22 uses).
const { Buffer, File } = require("buffer");

const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

// A MIME type's essence and parameters, or null when it does not parse.
function parseMime(text) {
  const [essence, ...rest] = String(text).split(";");
  const type = essence.trim().toLowerCase();
  if (!/^[^/\s]+\/[^/\s]+$/.test(type)) return null;
  const parameters = new Map();
  for (const part of rest) {
    const index = part.indexOf("=");
    if (index < 0) continue;
    const name = part.slice(0, index).trim().toLowerCase();
    let value = part.slice(index + 1).trim();
    if (value.startsWith('"')) value = value.slice(1, value.lastIndexOf('"') > 0 ? value.lastIndexOf('"') : undefined).replace(/\\(.)/g, "$1");
    if (name && !parameters.has(name)) parameters.set(name, value);
  }
  return { essence: type, parameters };
}

function failure() {
  return new TypeError("Failed to parse body as FormData.");
}

function startsWith(input, text, position) {
  for (let index = 0; index < text.length; index++) {
    if (input[position + index] !== text.charCodeAt(index)) return false;
  }
  return true;
}

function collect(input, position, keep) {
  const start = position.at;
  while (position.at < input.length && keep(input[position.at])) position.at++;
  return input.subarray(start, position.at);
}

function trimEnd(bytes) {
  let end = bytes.length;
  while (end > 0 && (bytes[end - 1] === 0x20 || bytes[end - 1] === 0x09)) end--;
  return bytes.subarray(0, end);
}

// A quoted name after `name="` or `filename="`, with %0A, %0D, and %22 decoded.
function parseName(input, position) {
  const bytes = collect(input, position, (byte) => byte !== 0x0a && byte !== 0x0d && byte !== 0x22);
  if (input[position.at] !== 0x22) return null;
  position.at++;
  return new TextDecoder().decode(bytes).replace(/%0A/gi, "\n").replace(/%0D/gi, "\r").replace(/%22/g, '"');
}

function parseHeaders(input, position) {
  const result = { name: null, filename: null, contentType: null, encoding: null };
  for (;;) {
    if (input[position.at] === 0x0d && input[position.at + 1] === 0x0a) return result.name === null ? null : result;
    const rawName = collect(input, position, (byte) => byte !== 0x0a && byte !== 0x0d && byte !== 0x3a);
    const headerName = Buffer.from(rawName).toString("latin1").replace(/^[\t ]+|[\t ]+$/g, "");
    if (!TOKEN.test(headerName) || input[position.at] !== 0x3a) return null;
    position.at++;
    collect(input, position, (byte) => byte === 0x20 || byte === 0x09);
    const rest = () => Buffer.from(trimEnd(collect(input, position, (byte) => byte !== 0x0a && byte !== 0x0d))).toString("latin1");
    switch (headerName.toLowerCase()) {
      case "content-disposition": {
        result.name = result.filename = null;
        if (!startsWith(input, 'form-data; name="', position.at)) return null;
        position.at += 17;
        result.name = parseName(input, position);
        if (result.name === null) return null;
        if (startsWith(input, "; filename", position.at)) {
          let check = position.at + 10;
          if (input[check] === 0x2a) {
            position.at++;
            check++;
          }
          if (input[check] !== 0x3d || input[check + 1] !== 0x22) return null;
          position.at += 12;
          result.filename = parseName(input, position);
          if (result.filename === null) return null;
        }
        break;
      }
      case "content-type":
        result.contentType = rest();
        break;
      case "content-transfer-encoding":
        result.encoding = rest();
        break;
      default:
        rest();
    }
    if (input[position.at] !== 0x0d && input[position.at + 1] !== 0x0a) return null;
    position.at += 2;
  }
}

function parseMultipart(input, boundaryText) {
  const boundary = Buffer.from(`--${boundaryText}`);
  const entries = [];
  const position = { at: 0 };
  while (input[position.at] === 0x0d && input[position.at + 1] === 0x0a) position.at += 2;
  let trailing = input.length;
  while (input[trailing - 1] === 0x0a && input[trailing - 2] === 0x0d) trailing -= 2;
  input = input.subarray(0, trailing);
  for (;;) {
    if (!input.subarray(position.at, position.at + boundary.length).equals(boundary)) throw failure();
    position.at += boundary.length;
    if ((position.at === input.length - 2 && startsWith(input, "--", position.at)) || (position.at === input.length - 4 && startsWith(input, "--\r\n", position.at))) {
      return entries;
    }
    if (input[position.at] !== 0x0d || input[position.at + 1] !== 0x0a) throw failure();
    position.at += 2;
    const headers = parseHeaders(input, position);
    if (headers === null) throw failure();
    position.at += 2;
    const boundaryIndex = input.indexOf(boundary.subarray(2), position.at);
    if (boundaryIndex === -1) throw failure();
    let body = input.subarray(position.at, boundaryIndex - 4);
    position.at += body.length;
    if (headers.encoding === "base64") body = Buffer.from(body.toString(), "base64");
    if (input[position.at] !== 0x0d || input[position.at + 1] !== 0x0a) throw failure();
    position.at += 2;
    let value;
    if (headers.filename !== null) {
      let type = headers.contentType === null ? "text/plain" : headers.contentType;
      if (!/^[\x00-\x7f]*$/.test(type)) type = "";
      value = new File([body], headers.filename, { type });
    } else {
      value = new TextDecoder().decode(body);
    }
    entries.push([headers.name, value]);
  }
}

// The entries of a body for `contentType`, or a TypeError like undici's.
function parseFormBody(bytes, contentType) {
  const mime = contentType === null ? null : parseMime(contentType);
  if (mime && mime.essence === "multipart/form-data") {
    const boundary = mime.parameters.get("boundary");
    if (boundary === undefined) throw failure();
    return parseMultipart(Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength), boundary);
  }
  if (mime && mime.essence === "application/x-www-form-urlencoded") {
    return [...new URLSearchParams(new TextDecoder().decode(bytes))];
  }
  throw new TypeError('Content-Type was not one of "multipart/form-data" or "application/x-www-form-urlencoded".');
}

module.exports = { parseFormBody };
