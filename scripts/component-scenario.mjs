#!/usr/bin/env node
// Usage: node scripts/component-scenario.mjs <upstream pi checkout with node_modules>
// Runs the component scenarios of tests/fixtures/extensions on upstream components and writes the results.
import { copyFileSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
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

// pi-rust ships more built-in themes than upstream (docs/parity/allowlist.json); upstream lists
// them as custom themes of a temporary agent directory, so its selector renders the same list.
const agentDir = mkdtempSync(join(tmpdir(), "pi-component-scenario-"));
mkdirSync(join(agentDir, "themes"));
const builtinThemes = join(dirname(fileURLToPath(import.meta.url)), "../crates/pi-coding-agent/src/theme/builtin");
for (const name of ["ghostty", "darcula-islands", "nord", "catppuccin"]) {
	copyFileSync(join(builtinThemes, `${name}.json`), join(agentDir, "themes", `${name}.json`));
}
process.env.PI_CODING_AGENT_DIR = agentDir;

const source = (path) => import(pathToFileURL(join(upstream, "packages", path)).href);
const { initTheme } = await source("coding-agent/src/modes/interactive/theme/theme.ts");
const { KeybindingsManager } = await source("coding-agent/src/core/keybindings.ts");
const { getKeybindings, resetCapabilitiesCache, setCapabilities, setKeybindings } = await source("tui/src/index.ts");
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
	LoginDialogComponent: "login-dialog",
	SessionSelectorComponent: "session-selector",
	ThemeSelectorComponent: "theme-selector",
	TreeSelectorComponent: "tree-selector",
	ExtensionEditorComponent: "extension-editor",
	FooterComponent: "footer",
	ModelSelectorComponent: "model-selector",
	SettingsSelectorComponent: "settings-selector",
	ToolExecutionComponent: "tool-execution",
};
const classes = {};
for (const [name, file] of Object.entries(FILES)) {
	classes[name] = (await source(`coding-agent/src/modes/interactive/components/${file}.ts`))[name];
}

Object.assign(classes, { getKeybindings, resetCapabilitiesCache, setCapabilities });

/** One line per top-level field and per rendered component, so the fixture stays short. */
async function write(name) {
	const { scenario } = await import(pathToFileURL(join(fixtures, `${name}.mjs`)).href);
	const { rendered, ...rest } = await scenario(classes);
	const field = ([key, value]) => `${JSON.stringify(key)}: ${JSON.stringify(value)}`;
	const renders = Object.entries(rendered).map((entry) => `\t\t${field(entry)}`).join(",\n");
	const top = Object.entries(rest).map((entry) => `\t${field(entry)}`).join(",\n");
	writeFileSync(join(fixtures, `${name}.json`), `{\n${top},\n\t"rendered": {\n${renders}\n\t}\n}\n`);
	console.log(`Wrote ${join(fixtures, `${name}.json`)}`);
}

await write("component_scenario");
await write("component_scenario_b");
await write("component_scenario_c");
process.exit(0);
