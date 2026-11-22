const dns = require("node:dns");
const dgram = require("node:dgram");
const path = require("node:path");
const { start } = require("./dns_server.cjs");
const mark = (text) => require("fs").appendFileSync(path.join(__dirname, "stage"), `${text}\n`);
const out = {};
const many = Array.from({ length: 40 }, (_, index) => ["@", "A", 60, `192.0.2.${index + 100}`]);
const site = (query) => {
  if (query.transport === "udp" && query.name.startsWith("tc")) return { tc: true, answers: many.slice(0, 1) };
  if (query.name === "tchang.test") return null;
  if (query.name === "tcsplit.test") return { split: true, answers: many };
  return { answers: query.name === "tcbig.test" ? many : [["@", "A", 60, "192.0.2.40"]] };
};
const fail = (promise) => promise.then((value) => ["resolved", value], (error) => [error.code, error.syscall, error.hostname, error.message]);
const seen = (server, name) => server.log.filter((entry) => entry.name === name).map((entry) => `${entry.transport}:${entry.from}`);
const server = await start(site);
const resolver = new dns.promises.Resolver({ timeout: 2000, tries: 1 });
resolver.setServers([server.address]);
mark("truncated");
out.tcRetry = await resolver.resolve4("tcbig.test");
out.tcRetryQueries = server.queries("tcbig.test");
out.tcSplit = (await resolver.resolve4("tcsplit.test", { ttl: true })).length;
out.tcSplitQueries = server.queries("tcsplit.test");
const hung = new dns.promises.Resolver({ timeout: 300, tries: 1 });
hung.setServers([server.address]);
out.tcHung = await fail(hung.resolve4("tchang.test"));
out.tcHungClosed = await Promise.race([
  server.waitFor((entry) => entry.event === "close" && server.log.indexOf(entry) > server.log.findIndex((item) => item.name === "tchang.test")).then(() => true),
  new Promise((resolve) => setTimeout(resolve, 3000, false)),
]);
mark("local address");
const bound = new dns.promises.Resolver({ timeout: 2000, tries: 1 });
bound.setServers([server.address]);
bound.setLocalAddress("127.0.0.1");
out.boundLoopback = [await bound.resolve4("tcbound.test"), seen(server, "tcbound.test")];
const unbindable = new dns.promises.Resolver({ timeout: 2000, tries: 1 });
unbindable.setServers([server.address]);
unbindable.setLocalAddress("192.0.2.1");
out.unbindable = await fail(unbindable.resolve4("unbindable.test"));
out.unbindableQueries = server.queries("unbindable.test");
const probe = dgram.createSocket("udp4");
const second = await new Promise((resolve) => {
  probe.once("error", () => resolve(false));
  probe.bind(0, "127.0.0.2", () => resolve(true));
});
probe.close();
if (second) {
  const other = new dns.Resolver({ timeout: 2000, tries: 1 });
  other.setServers([server.address]);
  other.setLocalAddress("127.0.0.2");
  out.boundSecond = await new Promise((resolve) => other.resolve4("tcsecond.test", (error, list) => resolve([error, list, seen(server, "tcsecond.test")])));
}
mark("ipv6");
const v6 = await start(site, "::1").catch(() => null);
if (v6) {
  const six = new dns.promises.Resolver({ timeout: 2000, tries: 1 });
  six.setServers([v6.address]);
  six.setLocalAddress("127.0.0.1", "::1");
  out.v6Servers = six.getServers().map((entry) => entry.replace(String(v6.port), "PORT"));
  out.v6Answer = [await six.resolve4("tcsix.test"), seen(v6, "tcsix.test")];
  await v6.close();
}
mark("failover");
const tagged = (tag, rcode) => start((query) => (rcode === null ? null : rcode ? { rcode } : { answers: [["@", "A", 60, tag]] }));
const [servfail, refused, silent, missing, good] = await Promise.all([tagged("", 2), tagged("", 5), tagged("", null), tagged("", 3), tagged("192.0.2.50")]);
const failover = (servers, options = { timeout: 5000, tries: 1 }) => {
  const created = new dns.promises.Resolver(options);
  created.setServers(servers.map((entry) => entry.address));
  return created;
};
out.servers = failover([servfail, refused, good]).getServers().length;
out.afterServfail = await fail(failover([servfail, good]).resolve4("one.test"));
out.afterRefused = await fail(failover([refused, good]).resolve4("two.test"));
out.afterSilent = await fail(failover([silent, good], { timeout: 1000, tries: 1 }).resolve4("three.test"));
out.allFail = await fail(failover([servfail, refused]).resolve4("four.test"));
out.allSilent = await fail(failover([silent, silent], { timeout: 300, tries: 1 }).resolve4("five.test"));
out.nxdomainStops = await fail(failover([missing, good]).resolve4("six.test"));
out.nxdomainGoodQueries = good.queries("six.test");
out.rcodeQueries = [servfail.queries("one.test"), good.queries("one.test"), refused.queries("two.test"), good.queries("two.test")];
out.silentQueries = [silent.queries("three.test"), good.queries("three.test")];
const callbackFailover = new dns.Resolver({ timeout: 5000, tries: 1 });
callbackFailover.setServers([servfail.address, good.address]);
out.callbackFailover = await new Promise((resolve) => callbackFailover.resolve4("seven.test", (error, list) => resolve([error && error.message, list])));
await Promise.all([servfail, refused, silent, missing, good, server].map((entry) => entry.close()));
return out;
