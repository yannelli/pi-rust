// Extension-to-host requests of native extensions: each maps onto a `pi.*`, `ctx.*`, or
// `ctx.ui.*` member of the JavaScript extension API.
import { complete } from "@earendil-works/pi-ai";
import { call, callAsync } from "./bridge.js";
import { rpcError } from "./native_rpc.js";

const isSignal = (value) =>
	typeof value === "object" && value !== null && typeof value.aborted === "boolean" && typeof value.addEventListener === "function";

/** `value` as JSON: functions and abort signals dropped, maps as objects, `undefined` as null. */
export function toJson(value) {
	if (value === undefined) return null;
	const text = JSON.stringify(value, (key, item) => {
		if (isSignal(item)) return undefined;
		if (item instanceof Map) return Object.fromEntries(item);
		if (item instanceof Set) return [...item];
		return item;
	});
	return text === undefined ? null : JSON.parse(text);
}

export const isObject = (value) => typeof value === "object" && value !== null && !Array.isArray(value);

/** RFC 7386 merge patch, applied to `target` in place. */
export function mergePatch(target, patch) {
	for (const [key, value] of Object.entries(patch)) {
		if (value === null) delete target[key];
		else if (isObject(value) && isObject(target[key])) mergePatch(target[key], value);
		else target[key] = isObject(value) ? mergePatch({}, value) : value;
	}
	return target;
}

const safe = (read, fallback = null) => {
	try {
		return read() ?? fallback;
	} catch {
		return fallback;
	}
};

/** A model without headers and keys. */
export function modelJson(model) {
	if (!model) return null;
	const { provider, id, name, api, baseUrl, compat, contextWindow, maxTokens, reasoning } = model;
	return { provider, id, name, api, baseUrl, compat: compat ?? null, contextWindow, maxTokens, reasoning };
}

function scopedJson(scoped) {
	return (Array.isArray(scoped) ? scoped : []).flatMap(({ model, thinkingLevel }) => {
		if (!model) return [];
		const entry = { provider: model.provider, id: model.id };
		if (thinkingLevel) entry.thinkingLevel = thinkingLevel;
		return [entry];
	});
}

export function footerJson() {
	const read = (kind) => safe(() => call("ui.footer", 0, kind));
	const footer = { gitBranch: read("gitBranch") };
	const stats = read("sessionStats");
	const runState = read("runState");
	if (stats !== null) footer.sessionStats = stats;
	if (runState !== null) footer.runState = runState;
	return footer;
}

/** The ctx snapshot sent with events, tool calls, commands, and shortcuts. */
export function snapshot(state, ctx) {
	const sessionManager = safe(() => ctx.sessionManager);
	const size = safe(() => ctx.mode) === "tui" ? safe(() => call("ui.terminalSize", 0)) : null;
	const rows = size?.[1];
	return {
		cwd: safe(() => ctx.cwd),
		mode: safe(() => ctx.mode),
		hasUI: safe(() => ctx.hasUI, false),
		model: modelJson(safe(() => ctx.model)),
		scopedModels: scopedJson(safe(() => ctx.scopedModels)),
		thinkingLevel: safe(() => ctx.thinkingLevel),
		sessionId: safe(() => sessionManager?.getSessionId()),
		sessionName: safe(() => sessionManager?.getSessionName()),
		sessionFile: safe(() => sessionManager?.getSessionFile()),
		isIdle: safe(() => ctx.isIdle(), true),
		isProjectTrusted: safe(() => ctx.isProjectTrusted(), false),
		contextUsage: toJson(safe(() => ctx.getContextUsage())),
		terminalWidth: size?.[0] ?? null,
		terminalHeight: Number.isInteger(rows) && rows > 0 ? rows : null,
		flags: Object.fromEntries(state.flags.map((name) => [name, toJson(safe(() => state.pi.getFlag(name)))])),
		footer: footerJson(),
	};
}

function findModel(ctx, name) {
	const slash = name.indexOf("/");
	if (slash <= 0) throw rpcError(-32602, `Model "${name}" must be "provider/id"`);
	const model = ctx.modelRegistry.find(name.slice(0, slash), name.slice(slash + 1));
	if (!model) throw rpcError(-32602, `Unknown model ${name}`);
	return model;
}

/** `extra` over `base`; a name in `extra` replaces every `base` name equal to it without case. */
function mergeHeaders(base, extra) {
	if (extra === undefined || extra === null) return { ...base };
	if (!isObject(extra) || Object.values(extra).some((value) => typeof value !== "string")) {
		throw rpcError(-32602, "headers must be an object of string values");
	}
	const headers = { ...base };
	for (const [name, value] of Object.entries(extra)) {
		const lower = name.toLowerCase();
		for (const key of Object.keys(headers)) if (key.toLowerCase() === lower) delete headers[key];
		headers[name] = value;
	}
	return headers;
}

async function llmComplete(ctx, params, signal) {
	mergeHeaders({}, params.headers);
	const model = params.model ? findModel(ctx, params.model) : ctx.model;
	if (!model) throw rpcError(-32602, "No model is selected");
	const auth = await ctx.modelRegistry.getApiKeyAndHeaders(model);
	if (!auth?.ok) throw new Error(auth?.error ?? `No credentials for ${model.provider}`);
	const messages = params.messages ?? [{ role: "user", content: String(params.prompt ?? ""), timestamp: Date.now() }];
	const headers = mergeHeaders(auth.headers ?? {}, params.headers);
	const options = { apiKey: auth.apiKey, headers, signal };
	for (const key of ["maxTokens", "temperature", "reasoning"]) if (params[key] !== undefined) options[key] = params[key];
	const message = await complete(model, { systemPrompt: params.systemPrompt, messages }, options);
	if (message?.stopReason === "error") throw new Error(message.errorMessage ?? "Completion failed");
	const text = (message?.content ?? []).filter((block) => block.type === "text").map((block) => block.text).join("");
	return { message: toJson(message), text };
}

const SCOPES = new Set(["global", "project"]);

/** `settings.set`: one extension namespace of the global or the trusted project settings. */
function settingsSet(ctx, { namespace, value, scope = "global" }) {
	if (typeof namespace !== "string" || namespace === "") throw rpcError(-32602, "namespace must be a non-empty string");
	if (!SCOPES.has(scope)) throw rpcError(-32602, `scope must be "global" or "project", not ${JSON.stringify(scope)}`);
	if (scope === "project" && !ctx.isProjectTrusted()) {
		throw rpcError(-32003, "The project is not trusted; pi does not write its settings");
	}
	try {
		call("action.setSettings", 0, scope, namespace, value === undefined ? null : value);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		throw rpcError(message.includes("not an extension settings namespace") ? -32602 : -32603, message);
	}
	return null;
}

const TARGETS = {
	pi: (state) => state.pi,
	ctx: (state) => state.currentCtx(),
	"ctx.ui": (state) => state.currentCtx().ui,
	"ctx.sessionManager": (state) => state.currentCtx().sessionManager,
	"ctx.modelRegistry": (state) => state.currentCtx().modelRegistry,
};

async function piCall(state, { target, method, args = [] }) {
	const resolve = TARGETS[target];
	if (!resolve) throw rpcError(-32602, `Unknown target ${target}`);
	const object = resolve(state);
	const member = object?.[method];
	if (member === undefined) throw rpcError(-32601, `${target}.${method} does not exist`);
	if (typeof member !== "function") return toJson(member);
	return toJson(await member.apply(object, args));
}

const CTX_METHODS = new Set([
	"ui.notify",
	"ui.select",
	"ui.confirm",
	"ui.input",
	"ui.editor",
	"ui.setStatus",
	"ui.setTitle",
	"ui.setEditorText",
	"ui.getEditorText",
	"ui.writeTerminal",
	"ui.setWidget",
	"ui.setFooter",
	"ui.custom",
	"session.getEntries",
	"session.getBranch",
	"model.get",
	"model.set",
	"model.find",
	"ctx.compact",
	"ctx.abort",
	"ctx.isIdle",
	"ctx.getContextUsage",
	"ctx.getSystemPrompt",
	"llm.complete",
	"auth.getProviderAuth",
	"settings.set",
	"auth.login",
]);

/** Whether request `method` reads `state.currentCtx()`, so it waits for the first ctx. */
export const needsCtx = (method, params) => (method === "pi.call" ? params?.target !== "pi" : CTX_METHODS.has(method));

/** The request handlers; `state.currentCtx()` is the ctx of the newest event or command. */
export function hostMethods(state) {
	const { pi } = state;
	const ctx = () => state.currentCtx();
	const ui = () => {
		const current = ctx();
		if (!current.hasUI) throw rpcError(-32001, "pi has no UI in this mode");
		return current.ui;
	};
	const dialog = (params, signal) => ({ timeout: params.timeout, signal });
	const done = () => null;
	return {
		"ui.notify": (p) => (ctx().ui.notify(String(p.message ?? ""), p.type), done()),
		"ui.select": async (p, signal) => toJson(await ui().select(p.title, p.options ?? [], dialog(p, signal))),
		"ui.confirm": async (p, signal) => Boolean(await ui().confirm(p.title, p.message ?? "", dialog(p, signal))),
		"ui.input": async (p, signal) => {
			const options = { ...dialog(p, signal), prefill: p.prefill, secret: Boolean(p.secret) };
			return toJson(await ui().input(p.title, p.placeholder, options));
		},
		"ui.editor": async (p) => toJson(await ui().editor(p.title, p.prefill)),
		"ui.setStatus": (p) => (ctx().ui.setStatus(p.key, p.text ?? undefined), done()),
		"ui.setTitle": (p) => (ctx().ui.setTitle(p.title), done()),
		"ui.setEditorText": (p) => (ctx().ui.setEditorText(p.text ?? ""), done()),
		"ui.getEditorText": () => ctx().ui.getEditorText(),
		"ui.writeTerminal": (p) => {
			const target = ctx().ui;
			if (typeof target.writeTerminal !== "function") return false;
			return Boolean(target.writeTerminal(String(p.data ?? ""), { tmuxPassthrough: Boolean(p.tmuxPassthrough) }));
		},
		"session.sendMessage": (p) => {
			const { deliverAs, triggerTurn, ...message } = p;
			pi.sendMessage({ display: true, ...message }, { deliverAs, triggerTurn });
			return done();
		},
		"session.sendUserMessage": (p) => (pi.sendUserMessage(p.content, { deliverAs: p.deliverAs }), done()),
		"session.appendEntry": (p) => (pi.appendEntry(p.customType, p.data), done()),
		"session.getEntries": () => toJson(ctx().sessionManager.getEntries()),
		"session.getBranch": (p) => toJson(ctx().sessionManager.getBranch(p.fromId)),
		"session.setName": (p) => (pi.setSessionName(p.name), done()),
		"session.getName": () => toJson(pi.getSessionName()),
		"model.get": () => modelJson(ctx().model),
		"model.find": (p) => {
			if (typeof p.provider !== "string" || typeof p.id !== "string") return null;
			return modelJson(ctx().modelRegistry.find(p.provider, p.id));
		},
		"model.set": async (p) => {
			const model = ctx().modelRegistry.find(p.provider, p.id);
			return model ? Boolean(await pi.setModel(model)) : false;
		},
		"thinking.get": () => toJson(pi.getThinkingLevel()),
		"thinking.set": (p) => (pi.setThinkingLevel(p.level), done()),
		"tools.getActive": () => toJson(pi.getActiveTools()),
		"tools.setActive": (p) => (pi.setActiveTools(p.names ?? []), done()),
		"tools.getAll": () => toJson(pi.getAllTools()),
		"commands.getAll": () => toJson(pi.getCommands()),
		"flags.get": (p) => toJson(pi.getFlag(p.name)),
		"settings.get": (p) => {
			const settings = toJson(pi.getSettings()) ?? {};
			return p.namespace ? (settings[p.namespace] ?? null) : settings;
		},
		"settings.set": (p) => settingsSet(ctx(), p),
		"ctx.compact": (p) =>
			new Promise((resolve, reject) =>
				ctx().compact({
					customInstructions: p.customInstructions,
					onComplete: (result) => resolve(toJson(result)),
					onError: reject,
				}),
			),
		"ctx.abort": () => (ctx().abort(), done()),
		"ctx.isIdle": () => Boolean(ctx().isIdle()),
		"ctx.getContextUsage": () => toJson(ctx().getContextUsage()),
		"ctx.getSystemPrompt": () => ctx().getSystemPrompt(),
		"llm.complete": (p, signal) => llmComplete(ctx(), p, signal),
		"auth.getProviderAuth": async (p) => toJson(await ctx().modelRegistry.getProviderAuth(p.provider)),
		"auth.login": async (p) => {
			if (!state.oauthProviders.has(p.provider)) {
				throw rpcError(-32602, `This extension registered no provider ${JSON.stringify(p.provider)} with oauth`);
			}
			if (ui() && ctx().mode !== "tui") throw rpcError(-32001, "pi has no login dialog in this mode");
			return Boolean(await callAsync("ui.login", 0, p.provider));
		},
		"usage.get": async (p) => toJson(await callAsync("models.usage", 0, p.provider ?? null, Boolean(p.refresh))),
		"events.emit": (p) => (pi.events.emit(p.channel, p.data), done()),
		"events.subscribe": (p) => {
			if (!state.subscriptions.has(p.channel)) {
				const forward = (data) => state.peer.notify("events.message", { channel: p.channel, data: toJson(data) });
				state.subscriptions.set(p.channel, pi.events.on(p.channel, forward));
			}
			return done();
		},
		"events.unsubscribe": (p) => {
			state.subscriptions.get(p.channel)?.();
			state.subscriptions.delete(p.channel);
			return done();
		},
		exec: async (p, signal) => {
			const options = { cwd: p.cwd, timeout: p.timeout, stdin: p.stdin, signal };
			return toJson(await pi.exec(p.command, p.args ?? [], options));
		},
		"pi.call": (p) => piCall(state, p),
	};
}
