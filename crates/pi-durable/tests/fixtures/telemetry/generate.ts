// Writes a golden SQLite telemetry fixture with the upstream TypeScript recorder. Development-time only.
import { rm } from "node:fs/promises";
import { registerHooks } from "node:module";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
// Usage: PI_UPSTREAM=<upstream pi checkout> node generate.ts <out-file>
const upstream = process.env.PI_UPSTREAM;
if (!upstream) throw new Error("set PI_UPSTREAM to an upstream pi checkout");
// Resolve pi-telemetry to its sources, so the upstream checkout needs no build.
const telemetrySource = pathToFileURL(join(upstream, "packages/telemetry/src/index.ts")).href;
registerHooks({
	resolve: (specifier, context, next) =>
		specifier === "@earendil-works/pi-telemetry"
			? { url: telemetrySource, shortCircuit: true }
			: next(specifier, context),
});
const { openNodeSqliteDatabase } = await import(join(upstream, "packages/chord/src/sqlite/node.ts"));
const { createSqliteTelemetry } = await import(join(upstream, "packages/chord/src/telemetry-sqlite/index.ts"));

const out = process.argv[2]!;
for (const suffix of ["", "-wal", "-shm"]) await rm(out + suffix, { force: true });
Date.now = () => 20_001;
const db = await openNodeSqliteDatabase(out);
const telemetry = await createSqliteTelemetry(db);
const attributes = { route: "/a", count: 3, ok: true, ratio: 0.25, tags: ["x", "y"], "10": "ten", "2": "two" };
let carrier: Record<string, string> | undefined;
await telemetry.startSpan({ name: "request", attributes }, async (root) => {
	root.addEvent("start", { step: 1, dropped: Number.NaN });
	carrier = telemetry.propagation.inject(root);
	await root
		.startSpan({ name: "query" }, () => {
			throw new TypeError("bad input");
		})
		.catch(() => {});
	await telemetry.startSpan({ name: "linked", links: [{ context: root, attributes: { reason: "retry" } }] }, () => {});
});
await telemetry.propagation.extract(carrier).startSpan({ name: "remote" }, (span) => span.setStatus({ status: "error" }));
const hits = telemetry.metrics.counter("hits", { unit: "By" });
hits.add(2, { method: "read", valid: true });
hits.add(3, { valid: true, method: "read" });
telemetry.metrics.histogram("latency", { unit: "ms" }).record(1.5, { b: 1, a: "x", "10": "ten", "2": "two" });
telemetry.metrics.upDownCounter("active").add(-1);
await telemetry.close();
await db.exec("VACUUM");
await db.close();
