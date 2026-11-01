#!/usr/bin/env node
// Usage: node scripts/project-context-scenario.mjs <upstream pi checkout with node_modules>
// Writes the upstream expectations of project_context_scenario.json and context_color_cases.json.
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const upstream = resolve(process.argv[2] ?? "");
if (!process.argv[2]) throw new Error("Usage: node scripts/project-context-scenario.mjs <upstream-root>");
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/tests/fixtures/extensions");

const loader = await import(pathToFileURL(join(upstream, "packages/coding-agent/src/core/resource-loader.ts")).href);
const { scenario } = await import(pathToFileURL(join(fixtures, "project_context_scenario.mjs")).href);
const result = await scenario({ loadProjectContextFiles: loader.loadProjectContextFiles });
const lines = Object.entries(result).map(([name, value]) => `\t${JSON.stringify(name)}: ${JSON.stringify(value)}`);
writeFileSync(join(fixtures, "project_context_scenario.json"), `{\n${lines.join(",\n")}\n}\n`);

// Each color case loads chalk fresh in a child with only the case's environment, and stdout piped (not a terminal).
const chalk = pathToFileURL(join(upstream, "node_modules/chalk/source/index.js")).href;
const version = JSON.parse(readFileSync(join(upstream, "node_modules/chalk/package.json"), "utf8")).version;
if (version !== "6.0.0") throw new Error(`expected chalk 6.0.0 at ${upstream}, found ${version}`);
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
console.log(`Wrote ${join(fixtures, "project_context_scenario.json")} and ${colorPath} with Node ${process.version}`);
process.exit(0);
