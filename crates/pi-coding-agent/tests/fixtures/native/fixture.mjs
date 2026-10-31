// A native extension for the bridge tests. pi runs it in a child process as a Node-style script
// (PI_JS_FORK=1) and it speaks the protocol of docs/extensions/native-protocol.md on stdio.
import readline from "node:readline";

let nextId = 1;
const pending = new Map();
const cancels = new Map();
const panelWaiters = new Map();
let initProbe;
let settleArmed = false;
let footerSeen = false;

const exitDelay = Number(process.env.NATIVE_EXIT_DELAY_MS ?? 0);
const exit = () => setTimeout(() => process.exit(0), exitDelay);
const send = (message) => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
const notify = (method, params) => send({ method, params });
const request = (method, params) =>
	new Promise((resolve, reject) => {
		const id = nextId++;
		pending.set(id, { resolve, reject });
		send({ id, method, params });
	});
const text = (value) => ({ content: [{ type: "text", text: value }] });

const registrations = {
	protocolVersion: 1,
	tools: [
		{
			name: "native_echo",
			label: "Native echo",
			description: "Echo the text; wait=true runs until cancelled",
			parameters: { type: "object", properties: { text: { type: "string" }, wait: { type: "boolean" } } },
		},
		{
			name: "native_draw",
			description: "Echo a string; the extension coerces other values and draws the rows",
			parameters: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
			prepareArguments: true,
			renderCall: true,
			renderResult: true,
		},
	],
	messageRenderers: ["native-card"],
	entryRenderers: ["native-note"],
	commands: [
		"native-ask", "native-panel", "native-llm", "native-crash", "native-write", "native-probe",
		"native-ctx", "native-find", "native-usage", "native-input", "native-exec", "native-exec-cap", "native-settle", "native-llm-headers", "native-log",
		"native-settings", "native-oauth", "native-auth", "native-login", "native-card", "native-order", "native-restyle", "native-render-count", "native-usage-slot",
	].map((name) => ({ name })),
	flags: [{ name: "native-flag", type: "string", default: "off" }],
	events: [
		"session_start", "before_agent_start", "tool_call", "tool_result", "message_end", "context",
		"agent_before_settle", "after_provider_response", ...(process.env.NATIVE_FOOTER === "off" ? [] : ["footer_changed"]),
	],
	autocomplete: [{ triggerCharacters: ["@"] }],
};

async function executeTool(id, { toolCallId, name, params }) {
	if (name === "native_draw") return text(`drew ${params.text}`);
	notify("tool.update", { toolCallId, partial: text(`working on ${params.text}`) });
	if (!params.wait) return text(`echo: ${params.text}`);
	await new Promise((resolve) => cancels.set(id, resolve));
	console.error("tool cancelled by host");
	throw Object.assign(new Error("cancelled by host"), { code: -32800 });
}

const textOf = (content) => (Array.isArray(content) ? content.map((block) => block.text ?? "").join("") : String(content ?? ""));

const answer = (method, params) => request(method, params).then(
	(result) => ({ result }),
	(error) => ({ error: error.message, code: error.code }),
);

async function initialize(params) {
	initProbe = {
		mode: params.mode,
		hasUI: params.hasUI,
		flag: await answer("pi.call", { target: "pi", method: "getFlag", args: ["native-flag"] }),
		commands: await answer("commands.getAll", {}),
	};
	return registrations;
}

const order = [];
let orderArmed = false;

async function onEvent({ name, event, ctx }) {
	if (name === "after_provider_response") {
		if (!orderArmed) return {};
		await new Promise((resolve) => setTimeout(resolve, 300));
		order.push(`after_provider_response ${event.status}`);
		return {};
	}
	if (name === "message_end" && orderArmed && event.message.role === "assistant") {
		orderArmed = false;
		order.push("message_end");
		void append("native-order", { order: [...order] });
	}
	if (name === "agent_before_settle") {
		if (!settleArmed) return {};
		settleArmed = false;
		const entry = { type: "custom", customType: "native-settle", data: { outcome: event.outcome, before: event.entries.length } };
		return { result: { entries: [...event.entries, entry], continue: false } };
	}
	if (name === "tool_result") {
		return event.input?.text === "patch!" ? { patch: { content: [{ type: "text", text: "patched result" }] } } : {};
	}
	if (name === "message_end") {
		const { role, content } = event.message;
		return role === "assistant" && textOf(content) === "done patch" ? { patch: { message: { content: [{ type: "text", text: "message patched" }] } } } : {};
	}
	if (name === "context") {
		if (!event.messages.some((message) => message.role === "user" && textOf(message.content) === "patch context")) return {};
		const extra = { role: "user", content: [{ type: "text", text: "context patched" }], timestamp: Date.now() };
		return { patch: { messages: [...event.messages, extra] } };
	}
	if (name === "session_start") {
		notify("ui.setWidget", { key: "native", lines: [`widget for ${ctx.mode}`, `flag=${ctx.flags["native-flag"]}`] });
		return {};
	}
	if (name === "before_agent_start") return { patch: { systemPromptOptions: { sections: { native: "From the native fixture." } } } };
	if (event.toolName !== "native_echo") return {};
	if (event.input.text === "blocked") return { result: { block: true, reason: "blocked by native" } };
	if (event.input.text === "array") return { patch: ["array patch"] };
	return { patch: { input: { text: `${event.input.text}!` } } };
}

const append = (customType, data) => request("session.appendEntry", { customType, data });

function suggest({ lines, cursorLine, cursorCol }) {
	const prefix = lines[cursorLine].slice(0, cursorCol).split(/\s/).pop();
	return { items: [{ value: "@native-one" }, { value: "@native-two" }], prefix, merge: true };
}

async function runCommand({ name, ctx }) {
	if (name === "native-ctx") {
		const model = ctx.model ?? {};
		const { scopedModels, terminalWidth, terminalHeight, isProjectTrusted } = ctx;
		await append("native-ctx", { scopedModels, terminalWidth, terminalHeight, isProjectTrusted, baseUrl: model.baseUrl, compat: model.compat, keys: Object.keys(model) });
	} else if (name === "native-find") {
		const found = await request("model.find", { provider: "local", id: "other" });
		const missing = await request("model.find", { provider: "local", id: "missing" });
		const noId = await answer("model.find", { provider: "local" });
		const noProvider = await answer("model.find", { id: "other", provider: 7 });
		await append("native-find", { found, missing, noId, noProvider });
	} else if (name === "native-usage") {
		const one = await request("usage.get", { provider: "cursor" });
		const all = await request("usage.get", { refresh: true });
		await append("native-usage", { one, all });
	} else if (name === "native-input") {
		const value = await request("ui.input", { title: "Token", placeholder: "tok", prefill: "ab", secret: true });
		await append("native-input", { value });
	} else if (name === "native-exec") {
		const { stdout, code } = await request("exec", { command: "cat", stdin: "piped text" });
		await append("native-exec", { stdout, code });
	} else if (name === "native-exec-cap") {
		const capped = await request("exec", { command: process.execPath, args: ["--version"], maxBuffer: 3 });
		const invalid = await answer("exec", { command: process.execPath, args: ["--version"], maxBuffer: -1 });
		await append("native-exec-cap", { capped, invalid: invalid.code });
	} else if (name === "native-log") {
		process.stderr.write("warn: careful\nerror: broke\nplain line\n");
	} else if (name === "native-settle") {
		settleArmed = true;
	} else if (name === "native-ask") {
		const answer = await request("ui.select", { title: "Pick one", options: ["red", "blue"] });
		await request("session.appendEntry", { customType: "native-answer", data: { answer } });
	} else if (name === "native-panel") {
		const closed = request("ui.custom", { surface: "p1", lines: ["Panel one"] });
		const key = await new Promise((resolve) => panelWaiters.set("p1", resolve));
		notify("ui.close", { surface: "p1", result: `closed by ${key}` });
		const { result } = await closed;
		await request("session.appendEntry", { customType: "native-panel", data: { result } });
	} else if (name === "native-llm") {
		const { text } = await request("llm.complete", { prompt: "Say hi", maxTokens: 50 });
		await request("session.appendEntry", { customType: "native-llm", data: { text } });
	} else if (name === "native-llm-headers") {
		const headers = { "X-Shared": "from extension", Authorization: "Bearer extension", "x-extra": "1" };
		const { text } = await request("llm.complete", { prompt: "Say hi", headers });
		const invalid = await answer("llm.complete", { prompt: "Say hi", headers: { "x-number": 1 } });
		const list = await answer("llm.complete", { prompt: "Say hi", headers: ["x"] });
		await append("native-llm-headers", { text, invalid, list });
	} else if (name === "native-write") {
		const wrote = await request("ui.writeTerminal", { data: "\u001b]0;native\u0007" });
		await request("session.appendEntry", { customType: "native-write", data: { wrote } });
	} else if (name === "native-probe") {
		await request("session.appendEntry", { customType: "native-probe", data: initProbe });
	} else if (name === "native-settings") {
		await settingsCommand();
	} else if (name === "native-oauth") {
		await oauthCommand();
	} else if (name === "native-auth") {
		await authCommand();
	} else if (name === "native-login") {
		const slot = await answer("auth.login", { provider: "anthropic-2" });
		const foreign = await answer("auth.login", { provider: "local" });
		await append("native-login", { slot, foreign });
	} else if (name === "native-card") {
		await request("session.sendMessage", { customType: "native-card", content: "teal card", display: true });
		await append("native-note", { color: "plum" });
	} else if (name === "native-usage-slot") {
		await append("native-usage-slot", await request("usage.get", { provider: "anthropic-2", refresh: true }));
	} else if (name === "native-restyle") {
		cardStyle = " bold";
	} else if (name === "native-render-count") {
		await append("native-render-count", { count: messageRenders });
	} else if (name === "native-order") {
		orderArmed = true;
		order.length = 0;
	} else if (name === "native-crash") {
		process.exit(3);
	}
	return null;
}

/** Settings round trip: the extension namespace, a pi key, and the project scope. */
async function settingsCommand() {
	const global = await answer("settings.set", { namespace: "nativeFixture", value: { color: "teal" } });
	const read = await request("settings.get", { namespace: "nativeFixture" });
	const project = await answer("settings.set", { namespace: "nativeFixture", scope: "project", value: { color: "plum" } });
	const merged = await request("settings.get", { namespace: "nativeFixture" });
	const piKey = await answer("settings.set", { namespace: "defaultModel", value: "other" });
	const badScope = await answer("settings.set", { namespace: "nativeFixture", scope: "team", value: 1 });
	await append("native-settings", { global, read, project, merged, piKey, badScope });
}

async function oauthCommand() {
	const models = [{ id: "slot-model", name: "Slot model", contextWindow: 1000, maxTokens: 100 }];
	const config = { name: "Anthropic #2", api: "anthropic-messages", baseUrl: "http://127.0.0.1:9", models, oauth: { builtin: "anthropic", name: "Anthropic #2" } };
	const registered = await answer("provider.register", { name: "anthropic-2", config });
	const unknown = await answer("provider.register", { name: "nope-2", config: { ...config, oauth: { builtin: "nope" } } });
	await append("native-oauth", { registered, unknown });
}

async function authCommand() {
	const auth = await answer("auth.getProviderAuth", { provider: "anthropic-2" });
	await append("native-auth", auth);
}

let cardStyle = "";
let messageRenders = 0;

const renders = {
	"render.toolCall": ({ args, width }) => ({ lines: [`CALL ${args.text} at ${width}`] }),
	"render.toolResult": ({ result, options, width }) => ({ lines: [`RESULT ${textOf(result.content)} at ${width}${options.isPartial ? " partial" : ""}`] }),
	"render.message": ({ message, width }) => {
		messageRenders++;
		return { lines: [`CARD ${textOf(message.content)} at ${width}${cardStyle}`] };
	},
	"render.entry": ({ entry, options, width }) => ({ lines: [`NOTE ${entry.data.color} at ${width} ${options.expanded ? "open" : "closed"}`] }),
};

const methods = {
	initialize,
	...renders,
	"tool.prepare": ({ args }) => ({ ...args, text: typeof args.text === "string" ? args.text : `coerced ${JSON.stringify(args.text)}` }),
	"tool.execute": executeTool,
	event: onEvent,
	"command.run": runCommand,
	"autocomplete.suggest": suggest,
	shutdown: () => null,
};

function onNotification({ method, params }) {
	if (method === "$/cancelRequest") cancels.get(params.id)?.();
	if (method === "ui.resize") notify("ui.update", { surface: params.surface, lines: [`width ${params.width} height ${params.height ?? "none"}`] });
	if (method === "ui.key") panelWaiters.get(params.surface)?.(`${params.data}/${params.key}`);
	if (method === "footer.changed" && params.footer.gitBranch === "feature-x" && !footerSeen) {
		footerSeen = true;
		void append("native-footer", params.footer);
	}
	if (method === "initialized") notify("ui.notify", { message: "native ready" });
	if (method === "exit") exit();
}

async function serve(message) {
	const handler = methods[message.method];
	try {
		const result = await (message.method === "tool.execute" ? handler(message.id, message.params) : handler(message.params));
		send({ id: message.id, result: result ?? null });
	} catch (error) {
		send({ id: message.id, error: { code: error.code ?? -32603, message: error.message } });
	}
}

readline.createInterface({ input: process.stdin }).on("line", (line) => {
	const message = JSON.parse(line);
	if (message.method && message.id !== undefined) void serve(message);
	else if (message.method) onNotification(message);
	else {
		const waiter = pending.get(message.id);
		pending.delete(message.id);
		if (message.error) waiter?.reject(Object.assign(new Error(message.error.message), { code: message.error.code }));
		else waiter?.resolve(message.result);
	}
}).on("close", exit);
