import { resolve, join } from "node:path";
import { call, callAsync, optional, registerCallback, releaseCallback } from "./bridge.js";
import { hostRuntime } from "./sdk_resources.js";
import { SessionManager, SettingsManager } from "./sdk_storage.js";

function unsupported(member) {
	throw Object.assign(new Error(`${member} is not supported in pi-rust`), { code: "ERR_PI_UNSUPPORTED" });
}

function checkOptions(options, supported, prefix) {
	for (const key of Object.keys(options)) {
		if (options[key] !== undefined && !supported.includes(key)) unsupported(`${prefix}.${key}`);
	}
}

function resourceOptions(loader) {
	if (!loader) return {};
	if (typeof loader._sdkOptions === "function") {
		const snapshot = loader._sdkOptions();
		return { ...snapshot, systemPrompt: snapshot.systemPrompt ?? "" };
	}
	for (const [method, key] of [["getSkills", "skills"], ["getPrompts", "prompts"], ["getAgentsFiles", "agentsFiles"], ["getExtensions", "extensions"]]) {
		const result = loader[method]?.();
		const entries = Array.isArray(result) ? result : result?.[key];
		if (entries?.length) unsupported(`createAgentSession.resourceLoader.${method}`);
	}
	return { systemPrompt: loader.getSystemPrompt?.() ?? "", appendSystemPrompt: loader.getAppendSystemPrompt?.() ?? [] };
}

export async function createAgentSession(options = {}) {
	checkOptions(options, ["cwd", "agentDir", "model", "thinkingLevel", "tools", "excludeTools", "noTools",
		"systemPrompt", "resourceLoader", "sessionManager", "settingsManager", "modelRuntime"], "createAgentSession");
	const modelRuntimeId = options.modelRuntime?._sdkRuntimeId;
	if (options.modelRuntime && typeof modelRuntimeId !== "number") unsupported("createAgentSession.modelRuntime");
	if (options.tools?.some((tool) => typeof tool !== "string")) unsupported("createAgentSession.tools.definition");
	if (options.systemPrompt !== undefined && typeof options.systemPrompt !== "string") {
		throw new TypeError("createAgentSession.systemPrompt must be a string");
	}
	const cwd = resolve(options.cwd ?? options.sessionManager?.getCwd() ?? process.cwd());
	const agentDir = resolve(options.agentDir ?? call("config.info").agentDir);
	const directoryName = `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`;
	const sessionManager = options.sessionManager ?? SessionManager.create(cwd, join(agentDir, "sessions", directoryName));
	const resources = resourceOptions(options.resourceLoader);
	const settingsManager = options.settingsManager ?? SettingsManager.create(cwd, agentDir);
	if (!sessionManager._sdkId) unsupported("createAgentSession.sessionManager");
	if (!settingsManager._sdkId) unsupported("createAgentSession.settingsManager");
	if (options.systemPrompt !== undefined) resources.systemPrompt = options.systemPrompt;
	const result = await callAsync("sdk.create", {
		cwd, agentDir, model: options.model, thinkingLevel: options.thinkingLevel,
		tools: options.tools, excludeTools: options.excludeTools, noTools: options.noTools,
		sessionManagerId: sessionManager._sdkId, settingsManagerId: settingsManager._sdkId, resources, modelRuntimeId,
	});
	const listeners = [];
	let disposed = false;
	let events;
	const assertActive = () => {
		if (disposed) throw new Error("AgentSession has been disposed");
	};
	const read = (member) => {
		assertActive();
		return optional(call("sdk.read", result.id, member));
	};
	const control = (name, ...args) => {
		assertActive();
		return optional(call("sdk.control", result.id, name, ...args));
	};
	const run = (name, ...args) => {
		assertActive();
		return callAsync(`sdk.${name}`, result.id, ...args).then(optional);
	};
	const persist = (options) => options?.persist === true;
	const stopEvents = () => {
		if (events === undefined) return;
		if (!disposed) call("sdk.control", result.id, "unsubscribe");
		releaseCallback(events);
		events = undefined;
	};
	const session = new Proxy({
		sessionManager,
		settingsManager,
		get messages() { return read("messages"); },
		get model() { return read("model"); },
		get thinkingLevel() { return read("thinkingLevel"); },
		get isStreaming() { return read("isStreaming"); },
		get isIdle() { return read("isIdle"); },
		get isCompacting() { return read("isCompacting"); },
		get sessionId() { return read("sessionId"); },
		get sessionFile() { return read("sessionFile"); },
		get sessionName() { return read("sessionName"); },
		get systemPrompt() { return read("systemPrompt"); },
		get state() {
			const state = read("state");
			return { ...state, pendingToolCalls: new Set(state.pendingToolCalls) };
		},
		get routedModel() { return read("routedModel"); },
		get pendingMessageCount() { return read("pendingMessageCount"); },
		get steeringMode() { return read("steeringMode"); },
		get followUpMode() { return read("followUpMode"); },
		get autoCompactionEnabled() { return read("autoCompactionEnabled"); },
		get retryAttempt() { return read("retryAttempt"); },
		get isRetrying() { return read("isRetrying"); },
		get autoRetryEnabled() { return read("autoRetryEnabled"); },
		get isBashRunning() { return read("isBashRunning"); },
		get hasPendingBashMessages() { return read("hasPendingBashMessages"); },
		getContextUsage: () => read("contextUsage"),
		getSteeringMessages: () => read("steeringMessages"),
		getFollowUpMessages: () => read("followUpMessages"),
		getAvailableThinkingLevels: () => read("availableThinkingLevels"),
		supportsThinking: () => read("supportsThinking"),
		getSessionStats: () => read("sessionStats"),
		getUserMessagesForForking: () => read("userMessagesForForking"),
		getLastAssistantText: () => read("lastAssistantText"),
		getActiveToolNames: () => read("activeToolNames"),
		getCallableToolNames: () => read("callableToolNames"),
		getAllTools: () => read("allTools"),
		modelRuntime: options.modelRuntime ?? hostRuntime(),
		subscribe(listener) {
			assertActive();
			if (typeof listener !== "function") throw new TypeError("Expected a session event listener");
			listeners.push(listener);
			if (events === undefined) {
				events = registerCallback((event) => {
					for (const each of [...listeners]) each(event);
				});
				control("subscribe", events);
			}
			return () => {
				const index = listeners.indexOf(listener);
				if (index !== -1) listeners.splice(index, 1);
				if (listeners.length === 0) stopEvents();
			};
		},
		async prompt(text, promptOptions = {}) {
			checkOptions(promptOptions, ["images", "expandPromptTemplates", "source", "streamingBehavior"], "AgentSession.prompt");
			const behavior = promptOptions.streamingBehavior;
			const streamingBehavior = behavior ? (behavior === "followUp" ? "followUp" : "steer") : undefined;
			await run("prompt", text, { ...promptOptions, streamingBehavior });
		},
		steer(text, images, queueOptions = {}) { return run("steer", text, images ?? null, queueOptions); },
		followUp(text, images, queueOptions = {}) { return run("followUp", text, images ?? null, queueOptions); },
		clearQueue: () => control("clearQueue"),
		async abort() { await run("abort"); },
		async waitForIdle() { await run("waitForIdle"); },
		async setModel(model, modelOptions) { await run("setModel", model, persist(modelOptions)); },
		async cycleModel(direction = "forward", modelOptions) { return control("cycleModel", direction, persist(modelOptions)); },
		setThinkingLevel(level, modelOptions) { control("setThinkingLevel", level, persist(modelOptions)); },
		cycleThinkingLevel: (modelOptions) => control("cycleThinkingLevel", persist(modelOptions)),
		setSteeringMode(mode) { control("setSteeringMode", mode); },
		setFollowUpMode(mode) { control("setFollowUpMode", mode); },
		compact: (customInstructions) => run("compact", customInstructions ?? null),
		abortCompaction() { control("abortCompaction"); },
		abortBranchSummary() { control("abortBranchSummary"); },
		setAutoCompactionEnabled(enabled) { control("setAutoCompactionEnabled", enabled === true); },
		abortRetry() { control("abortRetry"); },
		setAutoRetryEnabled(enabled) { control("setAutoRetryEnabled", enabled === true); },
		setCacheWarmingMode(mode) { control("setCacheWarmingMode", mode); },
		abortBash() { control("abortBash"); },
		setSessionName(name) { control("setSessionName", name); },
		refreshContext() { control("refreshContext"); },
		setActiveToolsByName(names) { control("setActiveToolsByName", names); },
		exportToJsonl: (outputPath) => control("exportToJsonl", outputPath ?? null),
		async exportToHtml(outputPath, exportOptions = {}) {
			checkOptions(exportOptions, [], "AgentSession.exportToHtml");
			return control("exportToHtml", outputPath ?? null);
		},
		dispose() {
			if (disposed) return;
			call("sdk.dispose", result.id);
			disposed = true;
			listeners.length = 0;
			stopEvents();
		},
	}, {
		get(target, property, receiver) {
			if (property === "then" || typeof property === "symbol") return undefined;
			if (property in target) return Reflect.get(target, property, receiver);
			unsupported(`AgentSession.${property}`);
		},
	});
	const extensionsResult = {
		extensions: [], errors: [],
		get runtime() { unsupported("createAgentSession.extensionsResult.runtime"); },
	};
	return { session, extensionsResult, modelFallbackMessage: optional(result.modelFallbackMessage) };
}
