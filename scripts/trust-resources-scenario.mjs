#!/usr/bin/env node
// Usage: node scripts/trust-resources-scenario.mjs <upstream pi checkout with node_modules>
// Runs tests/fixtures/extensions/trust_resources_scenario.mjs on upstream trust-manager.ts and writes the result.
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const upstream = resolve(process.argv[2] ?? "");
if (!process.argv[2]) throw new Error("Usage: node scripts/trust-resources-scenario.mjs <upstream-root>");
const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/tests/fixtures/extensions");

const trust = await import(pathToFileURL(join(upstream, "packages/coding-agent/src/core/trust-manager.ts")).href);
const { scenario } = await import(pathToFileURL(join(fixtures, "trust_resources_scenario.mjs")).href);
const result = await scenario({ hasTrustRequiringProjectResources: trust.hasTrustRequiringProjectResources });
const lines = Object.entries(result).map(([name, value]) => `\t${JSON.stringify(name)}: ${JSON.stringify(value)}`);
writeFileSync(join(fixtures, "trust_resources_scenario.json"), `{\n${lines.join(",\n")}\n}\n`);
console.log(`Wrote ${join(fixtures, "trust_resources_scenario.json")}`);
process.exit(0);
