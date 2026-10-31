#!/usr/bin/env node
// Usage: node scripts/ai-compat-scenario.mjs <upstream pi checkout with node_modules>
// Runs tests/fixtures/extensions/ai_compat_scenario.mjs on upstream pi-ai compat and writes the result.
import { writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const upstream = resolve(process.argv[2] ?? "");
if (!process.argv[2]) throw new Error("Usage: node scripts/ai-compat-scenario.mjs <upstream-root>");
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/tests/fixtures/extensions");

const PACKAGES = { "pi-ai": "ai", "pi-telemetry": "telemetry", "pi-tui": "tui", "pi-agent-core": "agent" };
registerHooks({
	resolve(specifier, context, next) {
		const match = /^@earendil-works\/(pi-[a-z-]+)(\/.*)?$/.exec(specifier);
		const dir = match && PACKAGES[match[1]];
		if (!dir) return next(specifier, context);
		const entry = match[2] ? match[2].slice(1) : "index";
		return next(pathToFileURL(join(upstream, "packages", dir, "src", `${entry}.ts`)).href, context);
	},
});

const compat = await import(pathToFileURL(join(upstream, "packages/ai/src/compat.ts")).href);
const { scenario } = await import(pathToFileURL(join(fixtures, "ai_compat_scenario.mjs")).href);
const { faux, images } = await scenario(compat);
const lines = [`\t"faux": ${JSON.stringify(faux)}`, `\t"images": ${JSON.stringify(images)}`];
writeFileSync(join(fixtures, "ai_compat_scenario.json"), `{\n${lines.join(",\n")}\n}\n`);
console.log(`Wrote ${join(fixtures, "ai_compat_scenario.json")}`);
process.exit(0);
