const { Resolver } = require("node:dns/promises");
const { start } = require("./dns_server.cjs");
const mark = (step) => require("node:fs").appendFileSync(require("node:path").join(__dirname, "stage"), step + "\n");
const address = [["@", "A", 60, "192.0.2.7"]];
const server = await start((query) => {
  if (query.name === "alias.test") return { answers: [["@", "CNAME", 20, "target.test"]] };
  if (query.name === "multi.test") return { answers: [
    ["@", "CNAME", 90, "first.test"], ["first.test", "CNAME", 20, "target.test"],
    ["target.test", "A", 60, "192.0.2.8"], ["other.test", "A", 80, "192.0.2.9"],
  ] };
  if (query.name === "tcp.test") return { tc: true, answers: address };
  return { answers: address };
});
const dropped = await start(() => null);
const resolver = new Resolver({ timeout: 300, tries: 1 });
const out = {};
const result = (request) => request.catch((error) => error.code);
try {
  resolver.setServers([server.address]);
  mark("aliases");
  for (const method of ["resolve4", "resolve6", "resolveCname", "resolveMx", "resolveNs", "resolvePtr", "resolveSoa", "resolveTxt", "resolveAny"]) {
    out[method] = await result(resolver[method]("alias.test"));
  }
  out.followups = server.queries("target.test").length;
  out.multiAddress = await result(resolver.resolve4("multi.test", { ttl: true }));
  out.multiCname = await result(resolver.resolveCname("multi.test"));
  out.multiAny = await result(resolver.resolveAny("multi.test"));
  mark("name lengths");
  for (const length of [61, 62, 63]) {
    const name = ["a".repeat(63), "b".repeat(63), "c".repeat(63), "d".repeat(length)].join(".");
    out["length" + length] = [await result(resolver.resolve4(name)), await result(resolver.resolve4(name + "."))];
  }
  mark("TCP TC and close");
  out.tcpTC = await result(resolver.resolve4("tcp.test"));
  await server.waitFor((event) => event.event === "close");
  out.tcpExchanges = server.queries("tcp.test");
  mark("concurrent TCP replies");
  const waiting = [];
  const burst = await start((query) => {
    if (query.transport === "udp") return { tc: true };
    return new Promise((finish) => {
      waiting.push({ query, finish });
      if (waiting.length === 8) {
        for (const item of [...waiting].reverse()) {
          const index = Number(item.query.name.split(".")[0].slice(1));
          item.finish({ answers: [["@", "A", 60, "192.0.2." + (index + 1)]] });
        }
      }
    });
  });
  try {
    const requests = new Resolver({ timeout: 5000, tries: 1 });
    requests.setServers([burst.address]);
    out.burstAnswers = await Promise.all(Array.from({length: 8}, (_, i) => requests.resolve4("q" + i + ".test")));
    await burst.waitFor((event) => event.event === "close");
    out.burstConnections = burst.log.filter((event) => event.event === "connect").length;
    out.burstIds = new Set(waiting.map(({ query }) => query.id)).size;
  } finally { await burst.close(); }
  mark("server health");
  const servers = [dropped.address, server.address];
  const order = () => resolver.getServers().map((value) => servers.indexOf(value));
  resolver.setServers(servers);
  out.initialOrder = order();
  out.first = await result(resolver.resolve4("first-query.test"));
  out.firstOrder = order();
  resolver.setServers(servers);
  out.afterSet = order();
  out.next = await result(resolver.resolve4("next-query.test"));
  out.droppedQueries = [dropped.queries("first-query.test"), dropped.queries("next-query.test")];
  resolver.setServers([server.address]);
  resolver.setServers(servers);
  out.afterRemoval = order();
  mark("immediate results");
  for (const [key, name, configured] of [
    ["empty", "normal.test", false], ["emptyBadName", "two..dots", false],
    ["badName", "two..dots", true], ["specialName", "private.ONION.", true],
  ]) {
    const local = new Resolver({timeout: 500, tries: 1});
    const addresses = configured ? [server.address] : [];
    local.setServers(addresses);
    const pending = result(local.resolve4(name));
    let changed = true;
    try { local.setServers(addresses); } catch (error) { changed = error.code; }
    local.cancel();
    out[key] = [await pending, changed, server.queries(name.toLowerCase().replace(/\.$/, "")).length];
  }
  return out;
} finally {
  await Promise.all([server.close(), dropped.close()]);
}
