#!/usr/bin/env node
// Usage: node scripts/markdown-scenario.mjs <upstream pi checkout with node_modules>
// Runs tests/fixtures/extensions/markdown_scenario.mjs on upstream pi-tui and writes markdown_scenario.json.
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const upstream = resolve(process.argv[2] ?? "");
if (!process.argv[2]) throw new Error("Usage: node scripts/markdown-scenario.mjs <upstream-root>");
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/tests/fixtures/extensions");

const tui = await import(pathToFileURL(join(upstream, "packages/tui/src/index.ts")).href);
const { scenario } = await import(pathToFileURL(join(fixtures, "markdown_scenario.mjs")).href);
const { rendered } = scenario(tui);
const lines = Object.entries(rendered).map(([key, value]) => `\t\t${JSON.stringify(key)}: ${JSON.stringify(value)}`);
const out = join(fixtures, "markdown_scenario.json");
const payload = `{\n\t"rendered": {\n${lines.join(",\n")}\n\t}\n}\n`;
if (Buffer.byteLength(payload) > 256 * 1024) throw new Error("Markdown reference output exceeded 256 KiB");
writeFileSync(out, payload);
console.log(`Wrote ${out}`);
