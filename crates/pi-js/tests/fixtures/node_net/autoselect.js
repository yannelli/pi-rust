const net = require("node:net");
const out = {};
const server = net.createServer((socket) => socket.destroy());
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const seen = [];
// The lookup answers later, as dns.lookup does, so the listeners below see every event.
const lookup = (list) => (host, options, callback) => {
  seen.push([options.all === true, options.family === undefined ? null : options.family]);
  setImmediate(callback, null, options.all ? list : list[0].address, options.all ? undefined : list[0].family);
};
// ::1 may be missing on a host, so its errors are not compared. Refusals on Windows take about
// 2 seconds, so a long time limit per address keeps them from counting as timeouts.
const shown = (arg) => (arg instanceof Error ? (arg.address === "::1" ? "error" : arg.code) : arg === port ? "port" : arg);
const run = (options) => new Promise((resolve) => {
  const events = [];
  const socket = net.connect({ port, host: "pi.test", autoSelectFamilyAttemptTimeout: 10000, ...options });
  for (const name of ["lookup", "connectionAttempt", "connectionAttemptFailed", "connectionAttemptTimeout", "connect"]) {
    socket.on(name, (...args) => events.push([name, ...args.map(shown)]));
  }
  socket.on("connect", () => socket.destroy());
  socket.on("error", (error) => events.push(["error", error.name, error.code, error.message.replace(String(port), "port"), (error.errors || []).length]));
  socket.on("close", () => {
    const tried = socket.autoSelectFamilyAttemptedAddresses;
    resolve([events, tried ? tried.map((entry) => entry.replace(String(port), "port")) : null]);
  });
});
const v4 = { address: "127.0.0.1", family: 4 };
const v6 = { address: "::1", family: 6 };
out.v6first = await run({ lookup: lookup([v6, v4]) });
out.single = await run({ lookup: lookup([v4]) });
out.family = await run({ family: 4, lookup: lookup([v4]) });
out.local = await run({ localAddress: "127.0.0.1", lookup: lookup([v4]) });
out.ip = await run({ host: "127.0.0.1" });
await new Promise((resolve) => server.close(resolve));
out.refused = await run({ lookup: lookup([v4, v4, v6, v4]) });
out.refusedSingle = await run({ lookup: lookup([v4]) });
net.setDefaultAutoSelectFamily(false);
out.defaults = [net.getDefaultAutoSelectFamily(), net.getDefaultAutoSelectFamilyAttemptTimeout()];
out.disabled = await run({ lookup: lookup([v4, v6]) });
out.optIn = await run({ autoSelectFamily: true, lookup: lookup([v4, v6]) });
net.setDefaultAutoSelectFamily(true);
try {
  net.connect({ port, host: "pi.test", autoSelectFamily: "yes" });
} catch (error) {
  out.invalid = error.code;
}
out.seen = seen;
return out;
