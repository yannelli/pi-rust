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
	const listeners = new Set();
	let disposed = false;
	let prompting = false;
	const assertActive = () => {
		if (disposed) throw new Error("AgentSession has been disposed");
	};
	const read = (member) => {
		assertActive();
		return optional(call("sdk.read", result.id, member));
	};
	const session = new Proxy({
		sessionManager,
		settingsManager,
		get messages() { return read("messages"); },
		get model() { return read("model"); },
		get thinkingLevel() { return read("thinkingLevel"); },
		get isStreaming() { return read("isStreaming"); },
		get sessionId() { return read("sessionId"); },
		get sessionFile() { return read("sessionFile"); },
		get systemPrompt() { return read("systemPrompt"); },
		get state() { return { model: read("model"), thinkingLevel: read("thinkingLevel") }; },
		get routedModel() { return read("routedModel"); },
		getContextUsage: () => read("contextUsage"),
		modelRuntime: options.modelRuntime ?? hostRuntime(),
		subscribe(listener) {
			assertActive();
			if (typeof listener !== "function") throw new TypeError("Expected a session event listener");
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
		async prompt(text, promptOptions = {}) {
			assertActive();
			checkOptions(promptOptions, ["images", "expandPromptTemplates", "source"], "AgentSession.prompt");
			if (prompting) throw new Error("Agent is already processing.");
			prompting = true;
			const callback = registerCallback((event) => {
				for (const listener of [...listeners]) listener(event);
			});
			try {
				await callAsync("sdk.prompt", result.id, text, promptOptions, callback);
			} finally {
				prompting = false;
				releaseCallback(callback);
			}
		},
		async abort() {
			assertActive();
			await callAsync("sdk.abort", result.id);
		},
		dispose() {
			if (disposed) return;
			call("sdk.dispose", result.id);
			disposed = true;
			listeners.clear();
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
