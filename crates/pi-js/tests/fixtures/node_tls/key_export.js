const tls = require("node:tls");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const failure = (error) => [error.code ?? null, error.name, error.message];
const context = (options) => {
  try {
    tls.createSecureContext({ cert: __CERT__, ...options });
    return "ok";
  } catch (error) {
    return failure(error);
  }
};
out.contexts = {
  pkcs8: context({ key: __PKCS8__, passphrase: "pi-test" }),
  legacy: context({ key: __LEGACY__, passphrase: "pi-test" }),
  perKey: context({ key: [{ pem: __PKCS8__, passphrase: "pi-test" }] }),
  bufferKey: context({ key: Buffer.from(__LEGACY__), passphrase: Buffer.from("pi-test") }),
  missing: context({ key: __PKCS8__ }),
  wrong: context({ key: __PKCS8__, passphrase: "nope" }),
  legacyMissing: context({ key: __LEGACY__ }),
  legacyWrong: context({ key: __LEGACY__, passphrase: "nope" }),
};

const exported = [];
const server = tls.createServer({ key: __PKCS8__, passphrase: "pi-test", cert: __CERT__ }, (socket) => {
  exported.push([socket.exportKeyingMaterial(32, "EXPORTER-pi-test", Buffer.from("context")),
    socket.exportKeyingMaterial(16, "EXPORTER-pi-test")]);
  socket.on("error", () => {});
  socket.end();
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const port = server.address().port;
const pair = async (options) => {
  const client = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: __CERT__, ...options });
  await once(client, "secureConnect");
  const protocol = client.getProtocol();
  const mine = [client.exportKeyingMaterial(32, "EXPORTER-pi-test", Buffer.from("context")),
    client.exportKeyingMaterial(16, "EXPORTER-pi-test")];
  const other = client.exportKeyingMaterial(32, "EXPORTER-other", Buffer.from("context"));
  client.on("error", () => {});
  await once(client, "close");
  const theirs = exported.shift();
  return {
    protocol,
    buffer: Buffer.isBuffer(mine[0]),
    lengths: [mine[0].length, mine[1].length],
    match: [mine[0].equals(theirs[0]), mine[1].equals(theirs[1])],
    labelChanges: !other.equals(mine[0]),
    contextChanges: !mine[0].subarray(0, 16).equals(mine[1]),
  };
};
out.tls13 = await pair({});
out.tls12 = await pair({ maxVersion: "TLSv1.2" });

const client = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: __CERT__ });
await once(client, "secureConnect");
const invalid = (...args) => {
  try {
    client.exportKeyingMaterial(...args);
    return "ok";
  } catch (error) {
    return failure(error);
  }
};
out.invalid = {
  length: invalid("32", "EXPORTER-pi-test"),
  label: invalid(32, 7),
  context: invalid(32, "EXPORTER-pi-test", "context"),
  negative: invalid(-1, "EXPORTER-pi-test"),
  zero: invalid(0, "EXPORTER-pi-test"),
};
client.on("error", () => {});
await once(client, "close");
out.unconnected = (() => {
  try {
    new tls.TLSSocket().exportKeyingMaterial(32, "EXPORTER-pi-test");
    return "ok";
  } catch (error) {
    return failure(error);
  }
})();
server.close();
await once(server, "close");
return out;
