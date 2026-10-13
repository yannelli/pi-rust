// Extension contexts (upstream `ExtensionRunner.createContext` and friends). Every getter reads
// the session `target` through Rust at call time; target 0 is this runtime's own session, other
// targets are sessions that replaced it (`withSession`).
import { call, callAsync, errorMessage, optional, registerCallback, releaseCallback, signalFor, signalId } from "./bridge.js";
import { createUiContext } from "./ui.js";

/** `ctx.sessionManager`: read-only views of the session tree. */
export function createSessionManagerView(target) {
	const read = (method, ...args) => optional(call("session.read", target, method, ...args));
	return {
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
		getEntries: () => read("getEntries") ?? [],
		getTree: () => read("getTree") ?? [],
		getSessionName: () => read("getSessionName"),
	};
}

/** `ctx.modelRegistry`: the session's model runtime. */
export function createModelRegistry(target, makeStream) {
	const read = (method, ...args) => optional(call("models.read", target, method, ...args));
	return {
		refresh: (options) => callAsync("models.refresh", target, options ?? {}),
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
		getProvider: (provider) => read("getProvider", provider),
		getRegisteredProviderIds: () => read("getRegisteredProviderIds") ?? [],
		getApiKeyAndHeaders: (model) => callAsync("models.auth", target, model),
		getApiKeyForProvider: (provider) => callAsync("models.apiKey", target, provider).then(optional),
		getProviderAuth: (provider) => callAsync("models.providerAuth", target, provider).then(optional),
		stream: (model, context, options) => makeStream(target, model, context, options),
		streamSimple: (model, context, options) => makeStream(target, model, context, options),
		complete: (model, context, options) => makeStream(target, model, context, options).result(),
		completeSimple: (model, context, options) => makeStream(target, model, context, options).result(),
		classify: (model, context, options) =>
			callAsync("models.classify", target, model, context, { ...options, signal: signalId(options?.signal) }),
		generateImages: (model, context, options) =>
			callAsync("models.generateImages", target, model, context, { ...options, signal: signalId(options?.signal) }),
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
			return createSessionManagerView(target);
		},
		get modelRegistry() {
			active();
			return createModelRegistry(target, runner.makeStream);
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

async function replaceSession(runner, op, args, options) {
	const withSession = options?.withSession;
	const setupKey = options?.setup ? registerCallback(options.setup) : undefined;
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
