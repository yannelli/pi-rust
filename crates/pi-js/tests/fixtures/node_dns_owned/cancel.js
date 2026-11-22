const dns = require("node:dns");
const path = require("node:path");
const { start, pause } = require("./dns_server.cjs");
const mark = (text) => require("fs").appendFileSync(path.join(__dirname, "stage"), `${text}\n`);
const out = {};
const held = [];
const server = await start(async (query) => {
  if (query.transport === "udp" && query.name.startsWith("tc")) return { tc: true };
  if (query.name.includes("held") || query.name === "70.2.0.192.in-addr.arpa") {
    return new Promise((finish) => held.push({ name: query.name, finish }));
  }
  if (query.name === "slow.test") await pause(600);
  if (query.type === "TXT") return { answers: [["@", "TXT", 60, "fresh"]] };
  return { answers: [["@", "A", 60, "192.0.2.61"]] };
});
const shape = (error) => [error.name, error.code, error.errno ?? null, error.syscall, error.hostname, error.message, Object.keys(error)];
const closedAfter = (name) => server.waitFor((entry) => entry.event === "close" && server.log.indexOf(entry) > server.log.findIndex((item) => item.name === name && item.transport === "tcp"));
mark("tcp cancel");
const tcp = new dns.promises.Resolver({ timeout: 10000, tries: 1 });
tcp.setServers([server.address]);
const order = [];
const hung = tcp.resolve4("tcheld.test").then((value) => ["resolved", value], (error) => (order.push("rejected"), shape(error)));
await server.waitFor((entry) => entry.name === "tcheld.test" && entry.transport === "tcp");
order.push(String(tcp.cancel()));
await closedAfter("tcheld.test");
order.push("closed");
out.tcpCancelled = await hung;
out.tcpOrder = order;
out.tcpAfter = await tcp.resolve4("tcafter.test");
out.tcpAfterQueries = server.queries("tcafter.test");
mark("udp cancel");
const udp = new dns.promises.Resolver({ timeout: 200, tries: 3 });
udp.setServers([server.address]);
const pending = udp.resolve4("udpheld.test").catch(shape);
await server.waitFor((entry) => entry.name === "udpheld.test");
udp.cancel();
out.udpCancelled = await pending;
out.setServersAfterCancel = (() => {
  try {
    udp.setServers([server.address]);
    return "ok";
  } catch (error) {
    return error.code;
  }
})();
out.udpAfter = await udp.resolveTxt("fresh.test");
// The slow reply outlasts the 200 ms retry interval, so a retry of the cancelled query would show up.
const waiter = new dns.promises.Resolver({ timeout: 3000, tries: 1 });
waiter.setServers([server.address]);
await waiter.resolve4("slow.test");
out.udpRetries = server.queries("udpheld.test");
mark("per resolver");
const first = new dns.promises.Resolver({ timeout: 10000, tries: 1 });
const second = new dns.promises.Resolver({ timeout: 10000, tries: 1 });
first.setServers([server.address]);
second.setServers([server.address]);
const events = [];
const a = first.resolve4("aheld.test").then((value) => events.push(["first", value]), (error) => events.push(["first", error.code]));
const b = second.resolve4("bheld.test").then((value) => events.push(["second", value]), (error) => events.push(["second", error.code]));
const c = second.resolve4("tccheld.test").then((value) => events.push(["secondTcp", value]), (error) => events.push(["secondTcp", error.code]));
await Promise.all(["aheld.test", "bheld.test"].map((name) => server.waitFor((entry) => entry.name === name)));
await server.waitFor((entry) => entry.name === "tccheld.test" && entry.transport === "tcp");
first.cancel();
await a;
events.push("release");
for (const { name, finish } of held.splice(0)) {
  finish(name === "bheld.test" || name === "tccheld.test" ? { answers: [["@", "A", 60, "192.0.2.60"]] } : null);
}
await Promise.all([b, c]);
out.perResolver = [...events.slice(0, 2), Object.fromEntries(events.slice(2).sort())];
out.firstAfter = await first.resolve4("again.test");
mark("callback cancel");
const callback = new dns.Resolver({ timeout: 10000, tries: 1 });
callback.setServers([server.address]);
const calls = [];
const done = Promise.all([
  new Promise((resolve) => callback.resolve4("cheld.test", (error, value) => resolve(calls.push(["resolve4", shape(error), value === undefined])))),
  new Promise((resolve) => callback.resolveTxt("cheld.test", (error) => resolve(calls.push(["resolveTxt", error.code, error.syscall])))),
  new Promise((resolve) => callback.reverse("192.0.2.70", (error) => resolve(calls.push(["reverse", shape(error)])))),
  new Promise((resolve) => callback.resolve("cheld.test", "ANY", (error) => resolve(calls.push(["resolve", error.code, error.syscall])))),
]);
await server.waitFor((entry) => entry.name === "70.2.0.192.in-addr.arpa");
await Promise.all(["A", "TXT", "ANY"].map((type) => server.waitFor((entry) => entry.name === "cheld.test" && entry.type === type)));
callback.cancel();
calls.push("returned");
await done;
out.callbackCancel = calls;
out.callbackAfter = await new Promise((resolve) => callback.resolveTxt("txt.test", (error, value) => resolve([error, value])));
out.idleCancel = [String(callback.cancel()), String(new dns.Resolver().cancel())];
for (const { finish } of held.splice(0)) finish(null);
await server.close();
return out;
