#!/usr/bin/env node
// Usage: node scripts/component-scenario.mjs <upstream pi checkout with node_modules>
// Runs tests/fixtures/extensions/component_scenario.mjs on upstream components and writes the result.
import { writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const upstream = resolve(process.argv[2] ?? "");
if (!process.argv[2]) throw new Error("Usage: node scripts/component-scenario.mjs <upstream-root>");
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

const source = (path) => import(pathToFileURL(join(upstream, "packages", path)).href);
const { initTheme } = await source("coding-agent/src/modes/interactive/theme/theme.ts");
const { KeybindingsManager } = await source("coding-agent/src/core/keybindings.ts");
const { setKeybindings } = await source("tui/src/index.ts");
initTheme("dark");
setKeybindings(new KeybindingsManager());

const FILES = {
	ArminComponent: "armin",
	AssistantMessageComponent: "assistant-message",
	BashExecutionComponent: "bash-execution",
	BranchSummaryMessageComponent: "branch-summary-message",
	CompactionSummaryMessageComponent: "compaction-summary-message",
	CustomMessageComponent: "custom-message",
	ExtensionInputComponent: "extension-input",
	ExtensionSelectorComponent: "extension-selector",
	OAuthSelectorComponent: "oauth-selector",
	ShowImagesSelectorComponent: "show-images-selector",
	SkillInvocationMessageComponent: "skill-invocation-message",
	ThinkingSelectorComponent: "thinking-selector",
	UserMessageComponent: "user-message",
	UserMessageSelectorComponent: "user-message-selector",
};
const classes = {};
for (const [name, file] of Object.entries(FILES)) {
	classes[name] = (await source(`coding-agent/src/modes/interactive/components/${file}.ts`))[name];
}

const { scenario } = await import(pathToFileURL(join(fixtures, "component_scenario.mjs")).href);
const { rendered, ...rest } = scenario(classes);
const field = ([key, value]) => `${JSON.stringify(key)}: ${JSON.stringify(value)}`;
const renders = Object.entries(rendered).map((entry) => `\t\t${field(entry)}`).join(",\n");
const top = Object.entries(rest).map((entry) => `\t${field(entry)}`).join(",\n");
writeFileSync(join(fixtures, "component_scenario.json"), `{\n${top},\n\t"rendered": {\n${renders}\n\t}\n}\n`);
console.log(`Wrote ${join(fixtures, "component_scenario.json")}`);
process.exit(0);
