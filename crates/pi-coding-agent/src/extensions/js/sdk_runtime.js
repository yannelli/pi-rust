// Upstream `createExtensionRuntime`, `readStoredCredential`, and `resolveCliModel` for SDK use.
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { call, optional } from "./bridge.js";
import { unsupportedError } from "./unsupported.js";

const STALE_MESSAGE =
	"This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().";

/** Upstream `McpServerRegistry`. */
class McpServerRegistry {
	#servers = new Map();
	#changeListener;

	register(server) {
		this.#servers.set(server.name, server);
		this.#changeListener?.();
	}

	unregister(name, extensionPath) {
		if (this.#servers.get(name)?.extensionPath !== extensionPath) return;
		this.#servers.delete(name);
		this.#changeListener?.();
	}

	get(name) {
		return this.#servers.get(name);
	}

	list() {
		return [...this.#servers.values()].map((server) => ({ ...server, config: structuredClone(server.config) }));
	}

	setChangeListener(listener) {
		this.#changeListener = listener;
	}
}

/** Upstream `createExtensionRuntime`: the runtime of a `ResourceLoader` before a session binds it. */
export function createExtensionRuntime() {
	const notInitialized = () => {
		throw new Error("Extension runtime not initialized. Action methods cannot be called during extension loading.");
	};
	const state = {};
	const eventBusUnsubscribers = new Set();
	const runtime = {
		sendMessage: notInitialized,
		sendUserMessage: notInitialized,
		appendEntry: notInitialized,
		setSessionName: notInitialized,
		getSessionName: notInitialized,
		setLabel: notInitialized,
		getActiveTools: notInitialized,
		getAllTools: notInitialized,
		getSettings: notInitialized,
		setActiveTools: notInitialized,
		refreshTools: () => {},
		getCommands: notInitialized,
		setModel: () => Promise.reject(new Error("Extension runtime not initialized")),
		getThinkingLevel: notInitialized,
		setThinkingLevel: notInitialized,
		flagValues: new Map(),
		pendingProviderRegistrations: [],
		pendingNativeProviderRegistrations: [],
		mcpServers: new McpServerRegistry(),
		pendingVirtualModelRegistrations: [],
		createContext: notInitialized,
		assertActive: () => {
			if (state.staleMessage) throw new Error(state.staleMessage);
		},
		invalidate: (message) => {
			if (state.staleMessage) return;
			state.staleMessage = message ?? STALE_MESSAGE;
			for (const unsubscribe of eventBusUnsubscribers) unsubscribe();
			eventBusUnsubscribers.clear();
		},
		trackEventBusSubscription: (unsubscribe) => {
			let active = true;
			const trackedUnsubscribe = () => {
				if (!active) return;
				active = false;
				eventBusUnsubscribers.delete(trackedUnsubscribe);
				unsubscribe();
			};
			eventBusUnsubscribers.add(trackedUnsubscribe);
			return trackedUnsubscribe;
		},
		reportError: () => {},
		registerProvider: (name, config, extensionPath = "<unknown>") => {
			runtime.pendingProviderRegistrations.push({ name, config, extensionPath });
		},
		registerNativeProvider: (provider, extensionPath = "<unknown>") => {
			runtime.pendingNativeProviderRegistrations.push({ provider, extensionPath });
		},
		unregisterProvider: (name) => {
			runtime.pendingProviderRegistrations = runtime.pendingProviderRegistrations.filter((r) => r.name !== name);
			runtime.pendingNativeProviderRegistrations = runtime.pendingNativeProviderRegistrations.filter(
				(r) => r.provider.id !== name,
			);
		},
		registerVirtualModel: (definition, extensionPath = "<unknown>") => {
			runtime.pendingVirtualModelRegistrations.push({ definition, extensionPath });
		},
		unregisterVirtualModel: (provider, id) => {
			runtime.pendingVirtualModelRegistrations = runtime.pendingVirtualModelRegistrations.filter(
				({ definition }) => definition.provider !== provider || definition.id !== id,
			);
		},
	};
	return runtime;
}

function expandTilde(path) {
	if (path === "~") return homedir();
	return path.startsWith("~/") || (process.platform === "win32" && path.startsWith("~\\")) ? join(homedir(), path.slice(2)) : path;
}

/** Upstream `readStoredCredential`: one provider's entry of an `auth.json` file, read once. */
export function readStoredCredential(providerId, authPath = join(call("config.info").agentDir, "auth.json")) {
	try {
		const text = readFileSync(expandTilde(authPath), "utf-8");
		return JSON.parse(text.startsWith("\uFEFF") ? text.slice(1) : text)[providerId];
	} catch {
		return undefined;
	}
}

/** Upstream `resolveCliModel` over a pi-rust `ModelRuntime`. */
export function resolveCliModel(options) {
	const { modelRuntime, ...rest } = options;
	if (typeof modelRuntime?._sdkRuntimeId !== "number") throw unsupportedError("resolveCliModel.modelRuntime");
	const result = call("sdk.resolveCliModel", { ...rest, modelRuntimeId: modelRuntime._sdkRuntimeId });
	return {
		model: optional(result.model),
		thinkingLevel: optional(result.thinkingLevel),
		warning: optional(result.warning),
		error: optional(result.error),
	};
}
