// Builds, renders, and drives the upstream footer, model and settings selectors, tool execution,
// and extension editor from structural sessions, runtimes, and configs, under Node and in the host.
const ANSI = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[\]_][^\x07\x1b]*(?:\x07|\x1b\\)/g;
const NOW = 1767225600000;
const [DOWN, ENTER, ESCAPE] = ["\x1b[B", "\r", "\x1b"];
const CAPABILITIES = { images: "kitty", trueColor: true, hyperlinks: false };

const strip = (lines) => lines.map((line) => line.replace(ANSI, "").trimEnd());
const tick = () => new Promise((resolve) => setTimeout(resolve, 20));
const press = (target, keys) => {
	for (const key of keys) target.handleInput(key);
};

function renders(component) {
	const narrow = strip(component.render(40));
	component.invalidate?.();
	return { narrow, wide: strip(component.render(72)) };
}

const usage = (input, output, total) => ({
	input,
	output,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: input + output,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total },
});

const model = (id, name) => ({
	id,
	name,
	api: "openai-completions",
	provider: "example",
	baseUrl: "https://example.invalid/v1",
	reasoning: true,
	input: ["text"],
	cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 128000,
	maxTokens: 8192,
});

function footerParts(current) {
	const entries = [
		{ type: "message", id: "u1", parentId: null, timestamp: new Date(NOW).toISOString(), message: { role: "user", content: "hi", timestamp: NOW } },
		{
			type: "message",
			id: "a1",
			parentId: "u1",
			timestamp: new Date(NOW).toISOString(),
			message: { role: "assistant", content: [{ type: "text", text: "hello" }], api: current.api, provider: current.provider, model: current.id, usage: usage(1200, 340, 0.0123), stopReason: "stop", timestamp: NOW },
		},
	];
	const session = {
		sessionManager: {
			getEntries: () => entries,
			getEntryCount: () => entries.length,
			getLeafId: () => "a1",
			getSessionId: () => "s1",
			getCwd: () => "/work/project",
			getSessionName: () => "Demo",
		},
		state: { model: current, thinkingLevel: "medium" },
		model: current,
		routedModel: undefined,
		getContextUsage: () => ({ tokens: 51200, contextWindow: 128000, percent: 40 }),
		modelRuntime: { isUsingSubscription: () => false },
	};
	const footerData = { getGitBranch: () => "main", getExtensionStatuses: () => new Map([["ext", "ready"]]), getAvailableProviderCount: () => 1 };
	return [session, footerData];
}

function settingsConfig(models) {
	return {
		autoCompact: true, defaultModel: "example/alpha", currentModel: models[0], availableDefaultModels: models,
		showImages: true, imageWidthCells: 60, autoResizeImages: true, blockImages: false, enableSkillCommands: true,
		steeringMode: "one-at-a-time", followUpMode: "one-at-a-time", transport: "auto", sessionShareDestination: "auto",
		httpIdleTimeoutMs: 300000, cacheWarmingMode: "off", thinkingLevel: "medium",
		availableThinkingLevels: ["off", "low", "medium", "high"], modelThinkingLevels: {}, currentTheme: "dark",
		terminalTheme: "dark", availableThemes: ["dark", "light"], hideThinkingBlock: false, mermaidRenderingMode: "auto",
		showCacheMissNotices: true, collapseChangelog: false, enableInstallTelemetry: true, doubleEscapeAction: "tree",
		treeFilterMode: "default", showHardwareCursor: false, editorPaddingX: 0, outputPad: 1, autocompleteMaxVisible: 5,
		quietStartup: false, defaultProjectTrust: "ask", clearOnShrink: false, showTerminalProgress: true, tuiMode: "main",
		fullscreenExitOutput: "transcript", fullscreenScrollbar: "auto", fullscreenCopyOnSelect: true,
		fullscreenWheelScrollLines: 3, warnings: {},
	};
}

/** The scenario on Linux with fixed terminal capabilities, which key names and settings follow. */
export async function scenario(c) {
	const platform = Object.getOwnPropertyDescriptor(process, "platform");
	Object.defineProperty(process, "platform", { value: "linux", configurable: true });
	c.setCapabilities(CAPABILITIES);
	try {
		return await run(c);
	} finally {
		Object.defineProperty(process, "platform", platform);
		c.resetCapabilitiesCache();
	}
}

async function run(c) {
	const tui = { requestRender() {}, stop() {}, start() {}, terminal: { rows: 24, columns: 80 } };
	const events = [];
	const record = (name) => (...args) => events.push([name, ...args]);
	const rendered = {};
	const models = [model("alpha", "Alpha"), model("beta", "Beta"), model("gamma", "Gamma")];

	rendered.footer = renders(new c.FooterComponent(...footerParts(models[0])));

	const runtime = {
		getAvailableSnapshot: () => models,
		getModel: (provider, id) => models.find((entry) => entry.provider === provider && entry.id === id),
		getError: () => undefined,
		refresh: async () => ({ aborted: false, errors: new Map() }),
	};
	const selector = new c.ModelSelectorComponent(tui, models[0], runtime, [], (picked) => events.push(["model", picked.id]), record("modelCancel"));
	await tick();
	rendered.models = renders(selector);
	press(selector, [..."gam"]);
	rendered.modelsSearch = renders(selector);
	press(selector, [ENTER]);

	const settingsEvents = new Proxy({}, { get: (_target, name) => (...args) => events.push(["settings", name, ...args]) });
	const settings = new c.SettingsSelectorComponent(settingsConfig(models), settingsEvents);
	rendered.settings = renders(settings);
	press(settings.getSettingsList(), [ENTER, ESCAPE]);

	const tool = new c.ToolExecutionComponent("bash", "call-1", { command: "ls -la" }, {}, undefined, tui, "/work/project");
	rendered.toolPending = renders(tool);
	tool.updateResult({ content: [{ type: "text", text: "file-a\nfile-b" }], isError: false, durationMs: 1200 });
	tool.setExpanded(true);
	rendered.toolDone = renders(tool);

	const editor = new c.ExtensionEditorComponent(tui, c.getKeybindings(), "Edit note", "draft", record("edit"), record("editCancel"));
	rendered.editor = renders(editor);
	press(editor, ["!", ENTER]);
	const cancelled = new c.ExtensionEditorComponent(tui, c.getKeybindings(), "Edit note", "draft", record("edit"), record("editCancel"));
	press(cancelled, [ESCAPE]);

	return { rendered, events };
}
