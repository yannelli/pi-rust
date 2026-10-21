// Each client reads the server's data and closes at once, with the server's close_notify maybe
// still unread. Like libuv, the close drains it, so the server sees an orderly end and no reset.
const tls = require("node:tls");
const path = require("node:path");
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const errors = [];
const server = tls.createServer({ key: __KEY__, cert: __CERT__ }, (socket) => {
  socket.on("error", (error) => errors.push(["server", error.code, error.message]));
  socket.end("hi");
});
server.on("tlsClientError", (error) => errors.push(["tlsClientError", error.code]));
const where = path.join(__dirname, "tls.sock");
await new Promise((resolve) => server.listen(where, resolve));
const texts = new Set();
for (let i = 0; i < 20; i++) {
  const client = tls.connect({ path: where, rejectUnauthorized: false });
  client.on("error", (error) => errors.push(["client", error.code]));
  client.on("data", (chunk) => {
    texts.add(String(chunk));
    client.destroy();
  });
  await once(client, "close");
}
await new Promise((resolve) => server.close(resolve));
return { errors, texts: [...texts] };
