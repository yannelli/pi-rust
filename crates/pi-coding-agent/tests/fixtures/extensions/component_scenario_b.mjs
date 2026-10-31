// Builds, renders, and drives the upstream session, tree, and theme selectors and the login dialog
// from plain records and callbacks, on upstream under Node and in the extension host.
const ANSI = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[\]_][^\x07\x1b]*(?:\x07|\x1b\\)/g;
const HOUR = 3600000;
const USAGE = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};
const [UP, DOWN, ENTER, ESCAPE, CTRL_U] = ["\x1b[A", "\x1b[B", "\r", "\x1b", "\x15"];

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

function sessionRecords(now) {
	const session = (id, name, first, hours) => ({
		path: `/sessions/${id}.jsonl`,
		id,
		cwd: "/work",
		name,
		created: new Date(now - hours * HOUR),
		modified: new Date(now - hours * HOUR),
		messageCount: 2,
		firstMessage: first,
		allMessagesText: first,
	});
	return [session("one", undefined, "fix the parser", 1), session("two", "Release notes", "write release notes", 3)];
}

function treeRecords(now) {
	const timestamp = new Date(now).toISOString();
	const message = (role, text) =>
		role === "user"
			? { role, content: text, timestamp: now }
			: { role, content: [{ type: "text", text }], api: "faux", provider: "faux", model: "m", usage: USAGE, stopReason: "stop", timestamp: now };
	const node = (id, parentId, role, text, children = []) => ({
		entry: { type: "message", id, parentId, timestamp, message: message(role, text) },
		children,
	});
	return [node("u1", null, "user", "first question", [node("a1", "u1", "assistant", "first answer", [node("u2", "a1", "user", "second question")])])];
}

/** The scenario with `process.platform` set to Linux, since key names and click hints follow it. */
export async function scenario(c) {
	const platform = Object.getOwnPropertyDescriptor(process, "platform");
	Object.defineProperty(process, "platform", { value: "linux", configurable: true });
	try {
		return await run(c);
	} finally {
		Object.defineProperty(process, "platform", platform);
	}
}

async function run(c) {
	const tui = { requestRender() {}, terminal: { rows: 24, columns: 80 } };
	const events = [];
	const record = (name) => (...args) => events.push([name, ...args]);
	const now = Date.now();
	const rendered = {};

	const sessions = sessionRecords(now);
	const load = async () => sessions;
	const options = { keybindings: c.getKeybindings() };
	const picker = new c.SessionSelectorComponent(load, load, record("session"), record("sessionCancel"), record("sessionExit"), () => {}, options, sessions[0].path);
	await tick();
	rendered.sessions = renders(picker);
	press(picker, [..."release"]);
	rendered.sessionsSearch = renders(picker);
	press(picker, [ENTER]);

	const tree = new c.TreeSelectorComponent(treeRecords(now), "u2", 24, record("tree"), record("treeCancel"), record("label"));
	rendered.tree = renders(tree);
	press(tree, [UP, "L", ..."mark", ENTER]);
	rendered.treeLabeled = renders(tree);
	press(tree, [CTRL_U]);
	rendered.treeUserOnly = renders(tree);
	press(tree, [ENTER]);

	const themes = new c.ThemeSelectorComponent("dark", record("theme"), record("themeCancel"), record("preview"));
	rendered.themes = renders(themes);
	press(themes.getSelectList(), [DOWN, ENTER]);

	const login = new c.LoginDialogComponent(tui, "example", record("login"), "Example");
	login.showAuth("example-auth://device", "Open the page on another device");
	const secret = login.showPrompt("Enter API key", "sk-...");
	rendered.loginPrompt = renders(login);
	press(login, [..."sk-1", ENTER]);
	events.push(["secret", await secret]);
	login.showProgress("Signing in");
	rendered.loginProgress = renders(login);
	press(login, [ESCAPE]);

	return { rendered, events };
}
