#!/usr/bin/env node
// Usage: node scripts/project-trust-store-scenario.mjs <upstream pi checkout at the pin, with node_modules> --temp-parent <dir>
// Writes the ProjectTrustStore scenario and child-process goldens; on win32 the `.win32.json` and Windows-only ones.
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { arch, release } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const PIN = "35dfa757378bd7a8570670e99c2687bdd1039a06";
const PACKAGES = {
	"proper-lockfile": "4.1.2",
	"graceful-fs": "4.2.11",
	"signal-exit": "3.0.7",
	"proper-lockfile/node_modules/retry": "0.12.0",
};
const args = process.argv.slice(2);
const at = args.indexOf("--temp-parent");
const [root] = args;
const parent = at >= 0 ? args[at + 1] : undefined;
if (!root || root.startsWith("--") || !parent) {
	throw new Error("Usage: node scripts/project-trust-store-scenario.mjs <upstream-root> --temp-parent <dir>");
}
const [major, minor] = process.versions.node.split(".").map(Number);
if (major !== 22 || minor < 19) throw new Error(`Node 22.19 or later 22.x required (upstream engines), found ${process.version}`);
const win32 = process.platform === "win32";
const goldenName = (base) => `${base}${win32 ? ".win32" : ""}.json`;
const upstream = resolve(root);
const head = spawnSync("git", ["-C", upstream, "rev-parse", "HEAD"], { encoding: "utf8" }).stdout.trim();
if (head !== PIN) throw new Error(`upstream HEAD is ${head || "unknown"}, expected ${PIN}`);
const modules = join(upstream, "node_modules");
const versions = {};
for (const [name, version] of Object.entries(PACKAGES)) {
	const found = JSON.parse(readFileSync(join(modules, name, "package.json"), "utf8")).version;
	if (found !== version) throw new Error(`${name}: expected ${version}, found ${found}`);
	versions[name] = found;
}
const sha = (path) => createHash("sha256").update(readFileSync(path)).digest("hex");
const managerPath = join(upstream, "packages/coding-agent/src/core/trust-manager.ts");
const hashes = Object.fromEntries(
	[
		managerPath,
		join(upstream, "packages/coding-agent/src/utils/paths.ts"),
		join(modules, "proper-lockfile/lib/lockfile.js"),
		join(modules, "proper-lockfile/lib/adapter.js"),
		join(modules, "proper-lockfile/lib/mtime-precision.js"),
		join(modules, "signal-exit/index.js"),
	].map((path) => [path.slice(upstream.length + 1), sha(path)]),
);
const generator = { node: process.version, upstream: PIN, packages: versions, sha256: hashes };
if (win32) Object.assign(generator, { platform: process.platform, arch: arch(), release: release() });
const tempParent = realpathSync(resolve(parent));
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/tests/fixtures/extensions");

const manager = await import(pathToFileURL(managerPath).href);
const { scenario } = await import(pathToFileURL(join(fixtures, "project_trust_store_scenario.mjs")).href);
const result = await scenario({ ProjectTrustStore: manager.ProjectTrustStore }, { tempParent });
const writeCases = (name, cases) => {
	const lines = Object.entries({ _generator: generator, ...cases }).map(([key, value]) => `\t${JSON.stringify(key)}: ${JSON.stringify(value)}`);
	const golden = join(fixtures, name);
	writeFileSync(golden, `{\n${lines.join(",\n")}\n}\n`);
	console.log(`Wrote ${golden} (${Object.keys(cases).length} cases, Node ${process.version})`);
};
writeCases(goldenName("project_trust_store_scenario"), result);
if (win32) {
	const windows = await import(pathToFileURL(join(fixtures, "project_trust_store_windows_scenario.mjs")).href);
	writeCases("project_trust_store_windows_scenario.json", await windows.scenario({ ProjectTrustStore: manager.ProjectTrustStore }, { tempParent }));
}

// Each child case runs in a Node process of its own, so the parent sees its exit code, markers, and the lock after.
const exitModule = pathToFileURL(join(fixtures, "project_trust_store_exit.mjs")).href;
const { MODES } = await import(exitModule);
const children = { _generator: generator };
for (const mode of MODES) {
	const work = realpathSync(mkdtempSync(join(tempParent, `pi-trust-store-${mode}-`)));
	try {
		const agentDir = join(work, "agent");
		const reportPath = join(work, "report.json");
		const child = join(work, "child.mjs");
		writeFileSync(
			child,
			`const { ProjectTrustStore } = await import(${JSON.stringify(pathToFileURL(managerPath).href)});\n` +
				`const { run } = await import(${JSON.stringify(exitModule)});\n` +
				`await run({ ProjectTrustStore }, ${JSON.stringify({ agentDir, reportPath, mode })});\n`,
		);
		const done = spawnSync(process.execPath, [child], { cwd: work, encoding: "utf8", timeout: 60000 });
		if (done.error) throw done.error;
		const clean = (text) => text.split(JSON.stringify(work).slice(1, -1)).join("<work>").split(work).join("<work>");
		children[mode] = {
			exitCode: done.status,
			report: existsSync(reportPath) ? JSON.parse(clean(readFileSync(reportPath, "utf8"))) : null,
			returned: existsSync(`${reportPath}.returned`),
			finally: existsSync(`${reportPath}.finally`),
			lockAfterExit: existsSync(join(agentDir, "trust.json.lock")),
			trustFileAfterExit: existsSync(join(agentDir, "trust.json")),
		};
		console.log(`${mode}: child exit ${done.status}`);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
}
const exitGolden = join(fixtures, goldenName("project_trust_store_exit"));
writeFileSync(exitGolden, `${JSON.stringify(children, null, "\t")}\n`);
console.log(`Wrote ${exitGolden}`);
console.log(JSON.stringify(generator));
process.exit(0);
