// node:stream/consumers: collect a Node, web, or async-iterable stream.
const { Buffer } = require("buffer");

async function buffer(stream) {
  const chunks = [];
  for await (const chunk of stream) {
    chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));
  }
  return Buffer.concat(chunks);
}

async function text(stream) {
  const decoder = new TextDecoder();
  let result = "";
  for await (const chunk of stream) result += typeof chunk === "string" ? chunk : decoder.decode(chunk, { stream: true });
  return result + decoder.decode();
}

module.exports = {
  buffer,
  text,
  arrayBuffer: async (stream) => {
    const bytes = await buffer(stream);
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  },
  json: async (stream) => JSON.parse(await text(stream)),
  blob: async (stream) => new Blob([await buffer(stream)]),
};
