#!/usr/bin/env node
// Usage: node --experimental-strip-types scripts/tui-viewport-scenario.mjs <upstream pi checkout with node_modules>
// Runs tests/fixtures/extensions/tui_viewport_scenario.mjs on upstream pi-tui and writes the result.
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const upstream = resolve(process.argv[2] ?? "");
if (!process.argv[2]) throw new Error("Usage: node scripts/tui-viewport-scenario.mjs <upstream-root>");
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/tests/fixtures/extensions");

const tui = await import(pathToFileURL(join(upstream, "packages/tui/src/index.ts")).href);
const { scenario } = await import(pathToFileURL(join(fixtures, "tui_viewport_scenario.mjs")).href);
const result = await scenario(tui);
const lines = Object.entries(result).map(([name, value]) => `\t${JSON.stringify(name)}: ${JSON.stringify(value)}`);
writeFileSync(join(fixtures, "tui_viewport_scenario.json"), `{\n${lines.join(",\n")}\n}\n`);
console.log(`Wrote ${join(fixtures, "tui_viewport_scenario.json")}`);
process.exit(0);
