// internal/tls_wrap: a TLSSocket over an existing net.Socket or duplex stream. Ciphertext passes
// through JavaScript in both directions, like Node's JSStreamSocket path for streams.
const { Buffer } = require("buffer");
const { Socket } = require("internal/net_socket");
const nativeTls = native.tls;
const nativeNet = native.net;

const toBuffer = (chunk) => (typeof chunk === "string" ? Buffer.from(chunk) : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));

function wireIn(tlsSocket, chunk) {
  const wrap = tlsSocket._wrap;
  if (wrap.ended) return;
  if (wrap.feed === null) {
    wrap.buffered.push(chunk);
  } else if (chunk === null) {
    wrap.ended = true;
    nativeTls.feedEnd(wrap.feed);
  } else {
    nativeTls.feed(wrap.feed, chunk);
  }
}

// Take the parent's incoming bytes before anything else reads them. A net.Socket stops
// emitting 'data', as in Node, where the TLS layer takes over its handle.
function attach(tlsSocket, parent) {
  tlsSocket._wrap = { parent, feed: null, ready: false, buffered: [], ended: false };
  tlsSocket._parent = parent;
  const receive = (chunk) => wireIn(tlsSocket, chunk);
  const isNet = parent instanceof Socket;
  if (isNet) {
    const original = parent._onNative;
    parent._onNative = function (event, value, error) {
      if (event === "data") return receive(toBuffer(value));
      if (event === "end") return receive(null);
      return original.call(this, event, value, error);
    };
  }
  // Node feeds bytes the parent already buffered to the TLS layer first.
  let chunk;
  while (parent.readableLength > 0 && (chunk = parent.read()) !== null) receive(toBuffer(chunk));
  if (isNet) {
    parent._read();
  } else {
    parent.on("data", (data) => receive(toBuffer(data)));
    parent.on("end", () => receive(null));
  }
  parent.on("error", (error) => tlsSocket.destroy(error));
  parent.on("close", () => receive(null));
  tlsSocket.connecting = isNet && (parent.connecting || parent._handle === null);
  if (tlsSocket.connecting) {
    parent.once("connect", () => {
      tlsSocket.connecting = false;
      tlsSocket.emit("connect");
    });
  }
}

// Begin the handshake once the parent is connected. `options` builds the native TLS options.
function start(tlsSocket, options, isServer) {
  const wrap = tlsSocket._wrap;
  if (!wrap || wrap.feed !== null || tlsSocket.destroyed) return;
  if (tlsSocket.connecting) {
    tlsSocket.once("connect", () => start(tlsSocket, options, isServer));
    return;
  }
  let result;
  try {
    result = nativeTls.wrap(options(), isServer, (event, value, error) => tlsSocket._onNative(event, value, error));
  } catch (error) {
    process.nextTick(() => tlsSocket.destroy(error));
    return;
  }
  tlsSocket._handle = result.id;
  wrap.feed = result.feed;
  if (!tlsSocket._refed) nativeNet.ref(result.id, false);
  for (const chunk of wrap.buffered.splice(0)) wireIn(tlsSocket, chunk);
}

function wireOut(tlsSocket, event, value) {
  const parent = tlsSocket._wrap.parent;
  if (parent.destroyed || parent.writableEnded) return;
  if (event === "wire") parent.write(toBuffer(value));
  else parent.end();
}

// Writes wait for the handshake, which ends after the parent's own 'connect'.
function whenSecure(tlsSocket, callback) {
  const done = () => {
    tlsSocket.removeListener("secure", done);
    tlsSocket.removeListener("close", done);
    callback();
  };
  tlsSocket.once("secure", done);
  tlsSocket.once("close", done);
}

// Closing the TLS socket closes the stream under it, as in Node.
function release(tlsSocket) {
  const wrap = tlsSocket._wrap;
  if (wrap.feed !== null && !wrap.ended) nativeTls.feedEnd(wrap.feed);
  wrap.ended = true;
  if (!wrap.parent.destroyed) wrap.parent.destroy();
}

// The parent's addresses, for a socket whose native handle has none.
function parentInfo(tlsSocket, info) {
  const parent = tlsSocket._wrap.parent;
  if (!(parent instanceof Socket)) return info;
  return { ...info, local: parent._local, remote: parent._remote };
}

module.exports = { attach, start, wireOut, whenSecure, release, parentInfo };
