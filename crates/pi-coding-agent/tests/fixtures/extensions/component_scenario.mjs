// Builds, renders, and drives the standalone interactive components of upstream pi. The same code
// runs on upstream under Node (expected output) and in the extension host (`component_exports.rs`).
const ANSI = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[\]_][^\x07\x1b]*(?:\x07|\x1b\\)/g;
const NOW = 1767225600000;
const USAGE = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};
const DOWN = "\x1b[B";
const UP = "\x1b[A";
const ENTER = "\r";
const ESCAPE = "\x1b";

const strip = (lines) => lines.map((line) => line.replace(ANSI, "").trimEnd());

/** Visible lines at 40 and then 72 columns, so a resize re-lays the component out. */
function renders(component) {
	const narrow = strip(component.render(40));
	component.invalidate?.();
	return { narrow, wide: strip(component.render(72)) };
}

export function scenario(c) {
	const tui = { requestRender() {}, terminal: { rows: 24, columns: 80 } };
	const events = [];
	const record = (name) => (...args) => events.push([name, ...args]);
	const drive = (name, component, keys, target = component) => {
		for (const key of keys) target.handleInput(key);
		return [name, component];
	};

	const assistant = {
		role: "assistant",
		content: [
			{ type: "thinking", thinking: "Plan the answer" },
			{ type: "text", text: "Answer with **bold** text" },
		],
		api: "faux",
		provider: "faux",
		model: "m",
		usage: USAGE,
		stopReason: "stop",
		timestamp: NOW,
	};
	const messages = {
		user: new c.UserMessageComponent("Hello **world**"),
		assistant: new c.AssistantMessageComponent(assistant),
		custom: new c.CustomMessageComponent({ role: "custom", customType: "note", content: "Custom body", display: true, timestamp: NOW }),
		branch: new c.BranchSummaryMessageComponent({ role: "branchSummary", summary: "Branch summary text", fromId: "a", timestamp: NOW }),
		compaction: new c.CompactionSummaryMessageComponent({ role: "compactionSummary", summary: "Compacted text", tokensBefore: 1234, timestamp: NOW }),
		skill: new c.SkillInvocationMessageComponent({ name: "demo", location: "/skills/demo/SKILL.md", content: "Skill body", userMessage: "run it" }),
	};

	const bash = new c.BashExecutionComponent("ls -la", tui);
	bash.appendOutput("line one\nline two\n");
	bash.setComplete(0, false);
	messages.bash = bash;

	const showImages = new c.ShowImagesSelectorComponent(true, record("showImages"), record("showImagesCancel"));
	const userMessages = new c.UserMessageSelectorComponent(
		[{ id: "a", text: "first" }, { id: "b", text: "second" }],
		record("message"),
		record("messageCancel"),
	);
	const driven = [
		drive("selector", new c.ExtensionSelectorComponent("Pick one", ["alpha", "beta", "gamma"], record("select"), record("selectCancel")), [DOWN, ENTER]),
		drive("selectorCancel", new c.ExtensionSelectorComponent("Pick", ["only"], record("select"), record("selectCancel")), [ESCAPE]),
		drive("input", new c.ExtensionInputComponent("Name?", undefined, record("submit"), record("submitCancel"), { initialValue: "pre" }), ["x", ENTER]),
		drive("inputCancel", new c.ExtensionInputComponent("Name?", undefined, record("submit"), record("submitCancel")), [ESCAPE]),
		drive("thinking", new c.ThinkingSelectorComponent("medium", ["off", "low", "medium", "high"], record("thinking"), record("thinkingCancel")), [DOWN, ENTER]),
		drive("showImages", showImages, [DOWN, ENTER], showImages.getSelectList()),
		drive("userMessages", userMessages, [UP, ENTER], userMessages.getMessageList()),
		drive("oauth", new c.OAuthSelectorComponent("login", [{ id: "anthropic", name: "Anthropic", authType: "oauth" }, { id: "openai", name: "OpenAI", authType: "api_key" }], record("oauth"), record("oauthCancel")), [DOWN, ENTER]),
	];

	const armin = new c.ArminComponent(tui);
	const arminLines = strip(armin.render(40));
	armin.dispose();

	const rendered = Object.fromEntries(Object.entries(messages).map(([name, component]) => [name, renders(component)]));
	for (const [name, component] of driven) rendered[name] = renders(component);
	return {
		rendered,
		events,
		armin: [arminLines.length > 0, arminLines.every((line) => line.length <= 40)],
	};
}
