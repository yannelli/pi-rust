// After close(), the client keeps its socket open until the server ends it, so frames the server
// still sends cannot reach a closed socket and reset the connection.
const http2 = require("node:http2");
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const order = [];
const server = http2.createServer();
server.on("session", (session) => session.socket.on("end", () => order.push("server end")));
server.on("stream", (stream) => stream.respond({ ":status": 200 }, { endStream: true }));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const client = http2.connect(`http://127.0.0.1:${server.address().port}`);
client.socket.on("close", () => order.push("client socket close"));
const request = client.request({ ":path": "/" });
request.resume();
await once(request, "close");
client.close();
await once(client.socket, "close");
await new Promise((resolve) => server.close(resolve));
return { order };
