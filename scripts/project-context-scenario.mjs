#!/usr/bin/env node
// Usage: node scripts/project-context-scenario.mjs <upstream pi checkout with node_modules> --temp-parent <dir>
//   --filesystem case-insensitive|case-sensitive. Writes that filesystem's scenario golden (and, for case-insensitive, the color cases).
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const GOLDENS = {
	"case-insensitive": "project_context_scenario.json",
	"case-sensitive": "project_context_scenario.case_sensitive.json",
};
const args = process.argv.slice(2);
const option = (name) => {
	const at = args.indexOf(name);
	return at >= 0 ? args[at + 1] : undefined;
};
const [root] = args;
const parent = option("--temp-parent");
const filesystem = option("--filesystem");
if (!root || root.startsWith("--") || !parent || !(filesystem in GOLDENS)) {
	throw new Error("Usage: node scripts/project-context-scenario.mjs <upstream-root> --temp-parent <dir> --filesystem case-insensitive|case-sensitive");
}
const upstream = resolve(root);
const tempParent = realpathSync(resolve(parent));
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/tests/fixtures/extensions");

// The kind is checked on the volume the scenario uses: a file written in lower case, looked up in upper case.
const probeDir = mkdtempSync(join(tempParent, "pi-context-case-probe-"));
let actual;
try {
	writeFileSync(join(probeDir, "case-probe"), "");
	actual = existsSync(join(probeDir, "CASE-PROBE")) ? "case-insensitive" : "case-sensitive";
} finally {
	rmSync(probeDir, { recursive: true, force: true });
}
if (actual !== filesystem) throw new Error(`--filesystem ${filesystem}, but ${tempParent} is ${actual}`);

const loaderPath = join(upstream, "packages/coding-agent/src/core/resource-loader.ts");
const chalkVersion = JSON.parse(readFileSync(join(upstream, "node_modules/chalk/package.json"), "utf8")).version;
if (chalkVersion !== "6.0.0") throw new Error(`expected chalk 6.0.0 at ${upstream}, found ${chalkVersion}`);
const loader = await import(pathToFileURL(loaderPath).href);
const { scenario } = await import(pathToFileURL(join(fixtures, "project_context_scenario.mjs")).href);
const result = await scenario({ loadProjectContextFiles: loader.loadProjectContextFiles }, { tempParent });
const lines = Object.entries(result).map(([name, value]) => `\t${JSON.stringify(name)}: ${JSON.stringify(value)}`);
const golden = join(fixtures, GOLDENS[filesystem]);
writeFileSync(golden, `{\n${lines.join(",\n")}\n}\n`);
const loaderSha = createHash("sha256").update(readFileSync(loaderPath)).digest("hex");
console.log(`Wrote ${golden} (${filesystem}, temp parent ${tempParent}, ${Object.keys(result).length} cases)`);
console.log(`Node ${process.version}, resource-loader.ts sha256 ${loaderSha}, chalk ${chalkVersion}`);
if (filesystem !== "case-insensitive") process.exit(0);

// Each color case loads chalk fresh in a child with only the case's environment, and stdout piped (not a terminal).
const chalk = pathToFileURL(join(upstream, "node_modules/chalk/source/index.js")).href;
const colorPath = join(fixtures, "context_color_cases.json");
const { probes, cases } = JSON.parse(readFileSync(colorPath, "utf8"));
if (process.platform === "win32") throw new Error("generate on macOS or Linux: `outputs` are the non-win32 expectations");
const work = mkdtempSync(join(tmpdir(), "pi-context-color-"));
try {
	const body = `const probes = ${JSON.stringify(probes)};\nconst { default: chalk } = await import(${JSON.stringify(chalk)});\nprocess.stdout.write(JSON.stringify(probes.map((text) => chalk.yellow(text))));\n`;
	const probe = join(work, "probe.mjs");
	writeFileSync(probe, body);
	// win32Outputs: chalk's win32 branch, with the platform and Windows 10 release set before chalk first loads.
	const win32Probe = join(work, "probe-win32.mjs");
	writeFileSync(win32Probe, `import os from "node:os";\nObject.defineProperty(process, "platform", { value: "win32", configurable: true });\nos.release = () => "10.0.19045";\n${body}`);
	const run = (item, file) => {
		const result = spawnSync(process.execPath, [file, ...item.argv], { env: item.env, encoding: "utf8", timeout: 30000 });
		if (result.status !== 0) throw new Error(`${item.name}: exit ${result.status} ${result.error ?? ""} ${result.stderr}`);
		return JSON.parse(result.stdout);
	};
	for (const item of cases) {
		item.outputs = run(item, probe);
		if ("win32Outputs" in item) item.win32Outputs = run(item, win32Probe);
	}
} finally {
	rmSync(work, { recursive: true, force: true });
}
writeFileSync(colorPath, `${JSON.stringify({ probes, cases }, null, "\t")}\n`);
console.log(`Wrote ${colorPath}`);
process.exit(0);
