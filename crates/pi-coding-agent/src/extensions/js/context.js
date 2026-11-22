// Extension contexts (upstream `ExtensionRunner.createContext` and friends). Every getter reads
// the session `target` through Rust at call time; target 0 is this runtime's own session, other
// targets are sessions that replaced it (`withSession`).
import { call, callAsync, errorMessage, optional, registerCallback, releaseCallback, signalFor, signalId } from "./bridge.js";
import { rustOAuth } from "./provider_oauth.js";
import { createUiContext } from "./ui.js";

// pi-rust: the entries of this runtime's own session (target 0), and the same objects by id.
// `getEntries()` copies only the ones appended since its last call, and all of them again when
// Rust reports a new generation.
let ownEntries = { generation: null, entries: [], byId: new Map() };
// Message objects that `message_end` handlers saw, by the id of their entry: as upstream's entry,
// the copied entry holds the object. Weak, since only code that keeps the object can compare it.
const endedMessages = new Map();
let pruneAbove = 64;

/** The entry `entryId` of this runtime's session holds `message` once it is copied. */
export function linkEndedMessage(entryId, message) {
	endedMessages.set(entryId, new WeakRef(message));
	if (endedMessages.size <= pruneAbove) return;
	for (const [id, ref] of endedMessages) if (!ref.deref()) endedMessages.delete(id);
	pruneAbove = Math.max(64, endedMessages.size * 2);
}

function withEndedMessage(entry) {
	const ref = endedMessages.get(entry.id);
	if (!ref) return entry;
	// Entries arrive in message order, so earlier messages without an entry never get one.
	for (const id of endedMessages.keys()) {
		endedMessages.delete(id);
		if (id === entry.id) break;
	}
	const message = ref.deref();
	if (message && entry.type === "message" && entry.message?.role === message.role) entry.message = message;
	return entry;
}

function ownSessionEntries() {
	const { generation, entries } = ownEntries;
	const added = call("session.read", 0, "entriesSince", generation, entries.length);
	if (added.generation !== generation || added.start !== entries.length) {
		ownEntries = { generation: added.generation, entries: [], byId: new Map() };
	}
	for (const entry of added.entries) {
		const own = withEndedMessage(entry);
		ownEntries.entries.push(own);
		ownEntries.byId.set(own.id, own);
	}
	return ownEntries.entries;
}

/**
 * pi-rust: `event.context` of `turn_end` and `agent_before_settle` from a `boundary.plan` of this
 * runtime's session. As upstream, each `sourceEntry` is the object `getEntries()` returns, and an
 * entry's own message is the same object in `contextMessages` and `llmMessages`.
 */
export function boundaryContext(plan) {
	ownSessionEntries();
	const drafted = new Map(plan.sources.map((entry) => [entry.id, entry]));
	const source = (id) => drafted.get(id) ?? ownEntries.byId.get(id) ?? optional(call("session.read", 0, "getEntry", id));
	const contextEntries = plan.entries.map(({ id, messages }) => {
		const sourceEntry = source(id);
		return { sourceEntry, messages: messages ?? (sourceEntry?.message ? [sourceEntry.message] : []) };
	});
	const contextMessages = contextEntries.flatMap((entry) => entry.messages);
	const converted = new Map(plan.llm);
	const llmMessages = [];
	contextMessages.forEach((message, index) => {
		const llm = converted.has(index) ? converted.get(index) : message;
		if (llm) llmMessages.push(llm);
	});
	return { contextEntries, contextMessages, llmMessages, pendingMessages: plan.pendingMessages, canContinue: plan.canContinue };
}

/** The session tree and its concrete manager's custom-entry writer. */
export function createSessionManagerView(target, assertActive) {
	const read = (method, ...args) => optional(call("session.read", target, method, ...args));
	return {
		appendCustomEntry(customType, data) {
			assertActive?.();
			return call("session.write", target, "appendCustomEntry", customType, data ?? null);
		},
		getCwd: () => read("getCwd"),
		getSessionDir: () => read("getSessionDir"),
		getSessionId: () => read("getSessionId"),
		getSessionFile: () => read("getSessionFile"),
		getLeafId: () => read("getLeafId") ?? null,
		getLeafEntry: () => read("getLeafEntry"),
		getEntry: (id) => read("getEntry", id),
		getLabel: (id) => read("getLabel", id),
		getBranch: (fromId) => read("getBranch", fromId ?? null) ?? [],
		buildContextEntries: () => read("buildContextEntries") ?? [],
		buildSessionProjection: () => read("buildSessionProjection"),
		buildSessionContext: () => read("buildSessionContext"),
		getHeader: () => read("getHeader") ?? null,
		getEntries: () => (target === 0 ? ownSessionEntries().slice() : (read("getEntries") ?? [])),
		getTree: () => read("getTree") ?? [],
		getSessionName: () => read("getSessionName"),
	};
}

/** `ctx.modelRegistry`: the session's model runtime, or SDK runtime `target` under `group` "modelRuntime". */
export function createModelRegistry(target, makeStream, group = "models", assertActive) {
	const read = (method, ...args) => optional(call(`${group}.read`, target, method, ...args));
	return {
		refresh: async (options) => {
			const { signal, ...rest } = options ?? {};
			const result = await callAsync(`${group}.refresh`, target, { ...rest, signal: signalId(signal) });
			// Upstream `ModelsRefreshResult`: errors as a Map of provider id to Error.
			const errors = Object.entries(result?.errors ?? {}).map(([provider, message]) => [provider, new Error(message)]);
			return { aborted: Boolean(result?.aborted), errors: new Map(errors) };
		},
		getError: () => read("getError"),
		getAll: () => read("getAll") ?? [],
		getAvailable: () => read("getAvailable") ?? [],
		find: (provider, modelId) => read("find", provider, modelId),
		findOfType: (type, provider, modelId) => read("findOfType", type, provider, modelId),
		getModelsOfType: (type, provider) => read("getModelsOfType", type, provider ?? null) ?? [],
		getAvailableOfType: (type, provider) => read("getAvailableOfType", type, provider ?? null) ?? [],
		getModelOfType: (type, provider, modelId) => read("findOfType", type, provider, modelId),
		hasConfiguredAuth: (model) => Boolean(read("hasConfiguredAuth", model?.provider)),
		isUsingOAuth: (model) => Boolean(read("isUsingOAuth", model?.provider)),
		getProviderDisplayName: (provider) => read("getProviderDisplayName", provider) ?? provider,
		getProviderAuthStatus: (provider) => read("getProviderAuthStatus", provider),
		getProvider(provider) {
			const entry = read("getProvider", provider);
			if (entry?.auth?.oauth) {
				entry.auth.oauth = rustOAuth({ provider, group, target }, entry.auth.oauth, assertActive);
			}
			return entry;
		},
		getRegisteredProviderIds: () => read("getRegisteredProviderIds") ?? [],
		getApiKeyAndHeaders: (model) => callAsync(`${group}.auth`, target, model),
		getApiKeyForProvider: (provider) => callAsync(`${group}.apiKey`, target, provider).then(optional),
		getProviderAuth: (provider) => callAsync(`${group}.providerAuth`, target, provider).then(optional),
		getUsage: (provider, options) => callAsync(`${group}.usage`, target, provider ?? null, Boolean(options?.refresh)),
		stream: (model, context, options) => makeStream(target, model, context, options),
		streamSimple: (model, context, options) => makeStream(target, model, context, options),
		complete: (model, context, options) => makeStream(target, model, context, options).result(),
		completeSimple: (model, context, options) => makeStream(target, model, context, options).result(),
		classify: (model, context, options) =>
			callAsync(`${group}.classify`, target, model, context, { ...options, signal: signalId(options?.signal) }),
		generateImages: (model, context, options) =>
			callAsync(`${group}.generateImages`, target, model, context, { ...options, signal: signalId(options?.signal) }),
	};
}

/** The plain extension context; `runner` supplies the UI and the staleness check. */
export function createContext(runner, target = 0) {
	const active = () => {
		if (target === 0) runner.assertActive();
	};
	const read = (op, ...args) => {
		active();
		return optional(call(op, target, ...args));
	};
	const ui = createUiContext(target, (kind, title, run) => runner.withUIPrompt(kind, title, run));
	return {
		get ui() {
			active();
			return ui;
		},
		get mode() {
			return read("ctx.mode");
		},
		get hasUI() {
			return Boolean(read("ctx.hasUI"));
		},
		get cwd() {
			return read("ctx.cwd");
		},
		get sessionManager() {
			active();
			return runner.sessionManagerFor(target);
		},
		get modelRegistry() {
			active();
			return runner.modelRegistryFor(target);
		},
		get model() {
			return read("ctx.model");
		},
		get scopedModels() {
			return read("ctx.scopedModels") ?? [];
		},
		get thinkingLevel() {
			return read("ctx.thinkingLevel");
		},
		isIdle: () => Boolean(read("ctx.isIdle")),
		isProjectTrusted: () => Boolean(read("ctx.isProjectTrusted")),
		get signal() {
			return signalFor(read("ctx.signal"));
		},
		abort: () => read("ctx.abort"),
		hasPendingMessages: () => Boolean(read("ctx.hasPendingMessages")),
		shutdown: () => read("ctx.shutdown"),
		getContextUsage: () => read("ctx.contextUsage"),
		getContextBreakdown: () => read("ctx.contextBreakdown"),
		compact: (options) => {
			active();
			callAsync("ctx.compact", target, options?.customInstructions ?? null).then(
				(result) => options?.onComplete?.(result),
				(error) => options?.onError?.(error instanceof Error ? error : new Error(errorMessage(error))),
			);
		},
		getSystemPrompt: () => read("ctx.systemPrompt") ?? "",
	};
}

function copy(context) {
	return Object.defineProperties({}, Object.getOwnPropertyDescriptors(context));
}

/** The context of a tool call: `tools` and `executeTool()` run nested calls of `toolCallId`. */
export function createToolContext(runner, toolCallId, signal, target = 0) {
	const context = copy(createContext(runner, target));
	return Object.defineProperties(context, {
		tools: {
			get() {
				if (target === 0) runner.assertActive();
				return call("tools.callable", target);
			},
		},
		executeTool: {
			value: async (name, args, options = {}) => {
				if (target === 0) runner.assertActive();
				const updateKey = options.onUpdate ? registerCallback(options.onUpdate) : undefined;
				try {
					return await callAsync("tools.executeNested", target, toolCallId, name, args ?? {}, {
						signal: signalId(options.signal ?? signal),
						updateKey,
					});
				} finally {
					if (updateKey !== undefined) releaseCallback(updateKey);
				}
			},
		},
	});
}

/** The session manager a `newSession` setup writes to (upstream `SessionManager`). */
function createWritableSessionManager(target) {
	const write = (method, ...args) => call("session.write", target, method, ...args);
	return {
		...createSessionManagerView(target),
		appendMessage: (message) => write("appendMessage", message),
		appendCustomMessageEntry: (customType, content, display, details) =>
			write("appendCustomMessageEntry", customType, content, Boolean(display), details ?? null),
		appendSessionInfo: (name) => write("appendSessionInfo", name),
		appendLabelChange: (targetId, label) => write("appendLabelChange", targetId, label ?? null),
		appendModelChange: (provider, modelId) => write("appendModelChange", provider, modelId),
		appendThinkingLevelChange: (level) => write("appendThinkingLevelChange", level),
	};
}

async function replaceSession(runner, op, args, options) {
	const withSession = options?.withSession;
	const setup = options?.setup;
	const setupKey = setup ? registerCallback((target) => setup(createWritableSessionManager(target))) : undefined;
	try {
		const result = await callAsync(op, ...args, { ...options, setup: setupKey, withSession: undefined });
		if (!result.cancelled && withSession && result.target) {
			await withSession(createReplacedSessionContext(runner, result.target));
		}
		return { cancelled: Boolean(result.cancelled) };
	} finally {
		if (setupKey !== undefined) releaseCallback(setupKey);
	}
}

/** Commands also control the session (upstream `createCommandContext`). */
export function createCommandContext(runner, target = 0) {
	const context = copy(createContext(runner, target));
	const active = () => {
		if (target === 0) runner.assertActive();
	};
	context.getSystemPromptOptions = () => {
		active();
		return call("ctx.systemPromptOptions", target);
	};
	context.waitForIdle = () => {
		active();
		return callAsync("ctx.waitForIdle", target).then(() => undefined);
	};
	context.newSession = (options) => {
		active();
		return replaceSession(runner, "session.new", [target], options);
	};
	context.fork = (entryId, options) => {
		active();
		return replaceSession(runner, "session.fork", [target, entryId], options);
	};
	context.navigateTree = (targetId, options) => {
		active();
		return callAsync("session.navigateTree", target, targetId, options ?? {});
	};
	context.switchSession = (sessionPath, options) => {
		active();
		return replaceSession(runner, "session.switch", [target, sessionPath], options);
	};
	context.reload = () => {
		active();
		return callAsync("session.reload", target).then(() => undefined);
	};
	return context;
}

/** The context `withSession` receives: the replacement session, usable after the old one ends. */
export function createReplacedSessionContext(runner, target) {
	const context = createCommandContext(runner, target);
	context.sendMessage = (message, options) => callAsync("action.sendMessage", target, message, options ?? {});
	context.sendUserMessage = (content, options) =>
		callAsync("action.sendUserMessage", target, content, options ?? {});
	return context;
}
