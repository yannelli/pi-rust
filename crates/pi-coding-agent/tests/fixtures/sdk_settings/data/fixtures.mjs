// Settings layers for oracle cases. Keys follow settings-schema.ts at 35dfa757; values are inputs
// only, and every expected value comes from running upstream SettingsManager.

/** A global layer that sets every key an accessor reads, plus an unknown key. */
export const GLOBAL = {
	$schema: "https://example.invalid/settings.schema.json",
	lastChangelogVersion: "1.2.3",
	extensionChangelogVersions: { "ext-a": "0.1.0" },
	sessionDir: "~/oracle-sessions/",
	defaultProvider: "anthropic",
	defaultModel: "claude-oracle",
	steeringMode: "all",
	followUpMode: "all",
	theme: "dark",
	defaultThinkingLevel: "high",
	modelThinkingLevels: { "anthropic/claude-oracle": "low" },
	transport: "websocket",
	sessionShareDestination: "radius",
	compaction: {
		enabled: false,
		reserveTokens: 1000,
		keepRecentTokens: 2000,
		modelOverrides: { "anthropic/claude-oracle": { reserveTokens: 3000 } },
	},
	branchSummary: { reserveTokens: 4000, skipPrompt: true },
	retry: { enabled: false, maxRetries: 7, baseDelayMs: 10, maxAgentDelayMs: 99, provider: { maxRetryDelayMs: 1234 } },
	httpIdleTimeoutMs: 45000,
	websocketConnectTimeoutMs: 5000,
	cacheWarming: "idle",
	hideThinkingBlock: true,
	showCacheMissNotices: true,
	externalEditor: "oracle-editor --wait",
	shellPath: "~/bin/oracle-sh",
	quietStartup: "header",
	defaultProjectTrust: "always",
	shellCommandPrefix: "set -e",
	npmCommand: ["pnpm", "--silent"],
	collapseChangelog: true,
	enableInstallTelemetry: false,
	enableAnalytics: true,
	trackingId: "tracking-oracle",
	deviceId: "device-oracle",
	packages: ["npm:oracle-a", { source: "git:oracle-b", extensions: ["x"] }],
	extensions: ["./ext-global"],
	skills: ["./skills-global"],
	prompts: ["./prompts-global"],
	themes: ["./themes-global"],
	enableSkillCommands: false,
	thinkingBudgets: { low: 1000, high: 9000 },
	terminal: {
		showImages: false,
		imageWidthCells: 33.7,
		clearOnShrink: true,
		showTerminalProgress: true,
		images: "kitty",
		trueColor: false,
		hyperlinks: true,
	},
	tuiMode: "regular",
	fullscreenExitOutput: "resume-hint",
	fullscreenScrollbar: "always",
	fullscreenCopyOnSelect: false,
	fullscreenWheelScrollLines: 250,
	images: { autoResize: false, blockImages: true },
	enabledModels: ["anthropic/*", "openai/gpt-*"],
	defaultTools: ["read", "grep"],
	doubleEscapeAction: "fork",
	treeFilterMode: "no-tools",
	showHardwareCursor: true,
	editorPaddingX: 2,
	outputPad: 0,
	autocompleteMaxVisible: 9,
	markdown: { codeBlockIndent: "    ", mermaid: "final" },
	warnings: { anthropicExtraUsage: false },
	oracleUnknownKey: { nested: [1, "two"], keep: true },
};

/** A project layer that overrides some global keys, nests into objects, and adds tool deltas. */
export const PROJECT = {
	defaultModel: "claude-project",
	compaction: { keepRecentTokens: 2500 },
	terminal: { showImages: true },
	extensions: ["./ext-project"],
	defaultTools: ["+bash", "-grep"],
	theme: "themes/custom.json",
	oracleProjectOnly: "kept",
};

/** Values of the wrong type or out of range, read through each accessor's own fallback or throw. */
export const INVALID = {
	steeringMode: "",
	theme: 42,
	transport: "carrier-pigeon",
	sessionShareDestination: "ftp",
	compaction: { reserveTokens: -1, keepRecentTokens: 1.5, modelOverrides: { "anthropic/claude-oracle": "x" } },
	httpIdleTimeoutMs: "soon",
	websocketConnectTimeoutMs: -5,
	cacheWarming: "always",
	externalEditor: "   ",
	quietStartup: "yes",
	defaultProjectTrust: "maybe",
	terminal: { imageWidthCells: "wide", images: true },
	tuiMode: "inline",
	fullscreenExitOutput: "none",
	fullscreenScrollbar: "sometimes",
	fullscreenWheelScrollLines: "fast",
	defaultTools: "read",
	treeFilterMode: "everything",
	outputPad: 7,
	markdown: { mermaid: "live" },
};

/** Legacy shapes that `migrateSettings` rewrites on load. */
export const LEGACY = {
	queueMode: "all",
	websockets: true,
	skills: { enableSkillCommands: false, customDirectories: ["./legacy-skills"] },
	retry: { maxDelayMs: 777, provider: { other: 1 } },
};

/** Legacy shapes that `migrateSettings` keeps or drops instead of rewriting. */
export const LEGACY_KEPT = {
	queueMode: "all",
	steeringMode: "one-at-a-time",
	websockets: false,
	transport: "sse",
	skills: { customDirectories: [] },
	retry: { maxDelayMs: 5, provider: { maxRetryDelayMs: 9 } },
};

export const MODEL = { provider: "anthropic", id: "claude-oracle" };
export const OTHER_MODEL = { provider: "openai", id: "gpt-oracle" };
