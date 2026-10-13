// node:string_decoder: decode byte chunks without splitting multi-byte characters.
const { Buffer } = require("buffer");
const { TextDecoder } = require("internal/web");

function normalize(name) {
  const lower = String(name || "utf8").toLowerCase();
  if (lower === "utf-8") return "utf8";
  if (lower === "ucs2" || lower === "ucs-2" || lower === "utf-16le") return "utf16le";
  if (lower === "binary") return "latin1";
  if (!Buffer.isEncoding(lower)) {
    throw Object.assign(new TypeError(`Unknown encoding: ${name}`), { code: "ERR_UNKNOWN_ENCODING" });
  }
  return lower;
}

class StringDecoder {
  #decoder = null;
  #pending = Buffer.alloc(0);
  constructor(encoding) {
    this.encoding = normalize(encoding);
    if (this.encoding === "utf8") this.#decoder = new TextDecoder("utf-8", { ignoreBOM: true });
    if (this.encoding === "utf16le") this.#decoder = new TextDecoder("utf-16le", { ignoreBOM: true });
  }
  write(chunk) {
    if (typeof chunk === "string") return chunk;
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    if (this.#decoder) return this.#decoder.decode(bytes, { stream: true });
    if (this.encoding === "base64" || this.encoding === "base64url") {
      const joined = Buffer.concat([this.#pending, bytes]);
      const usable = joined.length - (joined.length % 3);
      this.#pending = joined.subarray(usable);
      return joined.subarray(0, usable).toString(this.encoding);
    }
    return bytes.toString(this.encoding);
  }
  end(chunk) {
    let text = chunk === undefined ? "" : this.write(chunk);
    if (this.#decoder) text += this.#decoder.decode();
    else if (this.#pending.length) {
      text += this.#pending.toString(this.encoding);
      this.#pending = Buffer.alloc(0);
    }
    return text;
  }
  text(chunk, start) {
    return this.write(chunk.subarray(start));
  }
}

module.exports = { StringDecoder };
