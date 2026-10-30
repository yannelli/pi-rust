// A native extension for the bridge tests. pi runs it in a child process as a Node-style script
// (PI_JS_FORK=1) and it speaks the protocol of docs/extensions/native-protocol.md on stdio.
import readline from "node:readline";

let nextId = 1;
const pending = new Map();
const cancels = new Map();
const panelWaiters = new Map();
let initProbe;

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
	],
	commands: ["native-ask", "native-panel", "native-llm", "native-crash", "native-write", "native-probe"].map((name) => ({ name })),
	flags: [{ name: "native-flag", type: "string", default: "off" }],
	events: ["session_start", "before_agent_start", "tool_call", "tool_result", "message_end", "context"],
};

async function executeTool(id, { toolCallId, params }) {
	notify("tool.update", { toolCallId, partial: text(`working on ${params.text}`) });
	if (!params.wait) return text(`echo: ${params.text}`);
	await new Promise((resolve) => cancels.set(id, resolve));
	console.error("tool cancelled by host");
	throw Object.assign(new Error("cancelled by host"), { code: -32800 });
}

const textOf = (content) => (Array.isArray(content) ? content.map((block) => block.text ?? "").join("") : String(content ?? ""));

async function initialize() {
	const answer = (method, params) => request(method, params).then(
		(result) => ({ result }),
		(error) => ({ error: error.message }),
	);
	initProbe = {
		flag: await answer("pi.call", { target: "pi", method: "getFlag", args: ["native-flag"] }),
		commands: await answer("commands.getAll", {}),
	};
	return registrations;
}

function onEvent({ name, event, ctx }) {
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

async function runCommand({ name }) {
	if (name === "native-ask") {
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
	} else if (name === "native-write") {
		const wrote = await request("ui.writeTerminal", { data: "\u001b]0;native\u0007" });
		await request("session.appendEntry", { customType: "native-write", data: { wrote } });
	} else if (name === "native-probe") {
		await request("session.appendEntry", { customType: "native-probe", data: initProbe });
	} else if (name === "native-crash") {
		process.exit(3);
	}
	return null;
}

const methods = {
	initialize,
	"tool.execute": executeTool,
	event: onEvent,
	"command.run": runCommand,
	shutdown: () => null,
};

function onNotification({ method, params }) {
	if (method === "$/cancelRequest") cancels.get(params.id)?.();
	if (method === "ui.resize") notify("ui.update", { surface: params.surface, lines: [`width ${params.width}`] });
	if (method === "ui.key") panelWaiters.get(params.surface)?.(params.data);
	if (method === "initialized") notify("ui.notify", { message: "native ready" });
	if (method === "exit") process.exit(0);
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
		if (message.error) waiter?.reject(new Error(message.error.message));
		else waiter?.resolve(message.result);
	}
}).on("close", () => process.exit(0));
