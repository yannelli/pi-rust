// Upstream `DefaultResourceLoader`, `ModelRegistry`, and `ModelRuntime` for SDK use. The loader
// resolves system prompts the way upstream does; child sessions read its `_sdkOptions()` snapshot.
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { call } from "./bridge.js";
import { createModelRegistry } from "./context.js";
import { SettingsManager } from "./sdk_storage.js";
import { sessionStream } from "./stream.js";
import { unsupportedError } from "./unsupported.js";

// Options whose resources only the upstream package manager and extension loader produce.
const UNSUPPORTED_OPTIONS = [
	"extensionFactories",
	"additionalExtensionPaths",
	"additionalThemePaths",
	"extensionsOverride",
	"skillsOverride",
	"promptsOverride",
	"themesOverride",
	"agentsFilesOverride",
];

function resolvePromptInput(input) {
	if (!input) return undefined;
	if (!existsSync(input)) return input;
	return readFileSync(input, "utf-8").replace(/^\uFEFF/, "");
}

export class DefaultResourceLoader {
	constructor(options) {
		for (const key of UNSUPPORTED_OPTIONS) {
			const value = options[key];
			if (value !== undefined && !(Array.isArray(value) && value.length === 0)) {
				throw unsupportedError(`DefaultResourceLoader.${key}`);
			}
		}
		this.cwd = resolve(options.cwd);
		this.agentDir = resolve(options.agentDir);
		this.configDirName = call("config.info").configDirName;
		this.settingsManager = options.settingsManager ?? SettingsManager.create(this.cwd, this.agentDir);
		this.options = options;
		this.systemPrompt = undefined;
		this.systemPromptSourcePath = undefined;
		this.appendSystemPrompt = [];
		this.appendSystemPromptSourcePaths = [];
		this.loaded = false;
	}

	discover(name) {
		const projectPath = join(this.cwd, this.configDirName, name);
		if (this.settingsManager.isProjectTrusted() && existsSync(projectPath)) return projectPath;
		const globalPath = join(this.agentDir, name);
		return existsSync(globalPath) ? globalPath : undefined;
	}

	async reload() {
		this.loadPrompts();
	}

	loadPrompts() {
		const { systemPromptOverride, appendSystemPromptOverride } = this.options;
		const systemSource = this.options.systemPrompt ?? this.discover("SYSTEM.md");
		const baseSystemPrompt = resolvePromptInput(systemSource);
		this.systemPrompt = systemPromptOverride ? systemPromptOverride(baseSystemPrompt) : baseSystemPrompt;
		this.systemPromptSourcePath = systemSource && existsSync(systemSource) ? resolve(systemSource) : undefined;

		let appendSources = this.options.appendSystemPrompt;
		if (!appendSources) {
			const discovered = this.discover("APPEND_SYSTEM.md");
			appendSources = discovered ? [discovered] : [];
		}
		const baseAppend = appendSources.map(resolvePromptInput).filter((text) => text !== undefined);
		this.appendSystemPrompt = appendSystemPromptOverride ? appendSystemPromptOverride(baseAppend) : baseAppend;
		this.appendSystemPromptSourcePaths = appendSources.filter((source) => existsSync(source)).map((source) => resolve(source));
		this.loaded = true;
	}

	/** Skills, prompts, themes, and context files load in the child session; only disabled kinds read here. */
	disabledOnly(flag, member, empty) {
		if (this.options[flag]) return empty;
		throw unsupportedError(`DefaultResourceLoader.${member}`);
	}

	getExtensions() {
		return this.disabledOnly("noExtensions", "getExtensions", { extensions: [], errors: [], runtime: undefined });
	}

	getSkills() {
		return this.disabledOnly("noSkills", "getSkills", { skills: [], diagnostics: [] });
	}

	getPrompts() {
		return this.disabledOnly("noPromptTemplates", "getPrompts", { prompts: [], diagnostics: [] });
	}

	getThemes() {
		return this.disabledOnly("noThemes", "getThemes", { themes: [], diagnostics: [] });
	}

	getAgentsFiles() {
		return this.disabledOnly("noContextFiles", "getAgentsFiles", { agentsFiles: [] });
	}

	getSystemPrompt() {
		return this.systemPrompt;
	}

	getSystemPromptSource() {
		return this.systemPromptSourcePath ? { path: this.systemPromptSourcePath } : undefined;
	}

	getAppendSystemPrompt() {
		return this.appendSystemPrompt;
	}

	getAppendSystemPromptSources() {
		return this.appendSystemPromptSourcePaths.map((path) => ({ path }));
	}

	extendResources(paths) {
		if (Object.values(paths ?? {}).some((value) => Array.isArray(value) && value.length > 0)) {
			throw unsupportedError("DefaultResourceLoader.extendResources");
		}
	}

	async loadProjectTrustExtensions() {
		throw unsupportedError("DefaultResourceLoader.loadProjectTrustExtensions");
	}

	/** The options a child session reads; loads the system prompts on first use, as `reload()` does. */
	_sdkOptions() {
		if (!this.loaded) {
			this.loadPrompts();
		}
		const options = this.options;
		return {
			cwd: this.cwd,
			agentDir: this.agentDir,
			systemPrompt: this.systemPrompt,
			appendSystemPrompt: this.appendSystemPrompt,
			noExtensions: Boolean(options.noExtensions),
			noSkills: Boolean(options.noSkills),
			noPromptTemplates: Boolean(options.noPromptTemplates),
			noThemes: Boolean(options.noThemes),
			noContextFiles: Boolean(options.noContextFiles),
			additionalSkillPaths: options.additionalSkillPaths ?? [],
			additionalPromptTemplatePaths: options.additionalPromptTemplatePaths ?? [],
			settingsManagerId: this.settingsManager._sdkId,
		};
	}
}

import { callAsync, optional, signalId } from "./bridge.js";

const RUNTIME_UNSUPPORTED = [
	"login", "logout", "registerProvider", "unregisterProvider", "registerNativeProvider", "registerVirtualModel",
	"unregisterVirtualModel", "getRegisteredProviderConfig", "getRegisteredNativeProvider",
	"getCompatibilityRequestConfig", "getPhysicalModel",
];

const unsupportedStream = () => {
	throw unsupportedError("ModelRuntime.stream");
};

function registryFor(id) {
	return createModelRegistry(id, id === 0 ? sessionStream : unsupportedStream, "modelRuntime");
}

/** Upstream `ModelRuntime` over native runtime `id` (0 is the host's); auth methods take provider ids. */
function runtimeFor(id) {
	const registry = registryFor(id);
	const read = (method, ...args) => optional(call("modelRuntime.read", id, method, ...args));
	const run = (name, ...args) => callAsync(`modelRuntime.${name}`, id, ...args);
	const runtime = {
		...registry,
		_sdkRuntimeId: id,
		getModel: (providerId, modelId) => registry.find(providerId, modelId),
		getModels: (providerId) => read("getModels", providerId ?? null) ?? [],
		getAllModels: (providerId) => read("getAllModels", providerId ?? null) ?? [],
		getAvailable: async (providerId) => (await run("getAvailable", providerId ?? null)) ?? [],
		getAllAvailable: async (providerId) => read("getAllAvailable", providerId ?? null) ?? [],
		getAvailableOfType: async (type, providerId) => registry.getAvailableOfType(type, providerId),
		getAvailableSnapshot: () => registry.getAvailable(),
		getProviders: () => read("getProviders") ?? [],
		hasConfiguredAuth: (providerId) => registry.hasConfiguredAuth({ provider: providerId }),
		isUsingOAuth: (providerId) => registry.isUsingOAuth({ provider: providerId }),
		isUsingSubscription: (providerId) => Boolean(read("isUsingSubscription", providerId)),
		setRuntimeApiKey: async (providerId, apiKey) => {
			await run("setRuntimeApiKey", providerId, apiKey);
		},
		removeRuntimeApiKey: async (providerId) => {
			await run("removeRuntimeApiKey", providerId);
		},
		listCredentials: async () => (await run("listCredentials")) ?? [],
	};
	for (const name of RUNTIME_UNSUPPORTED) {
		runtime[name] = () => {
			throw unsupportedError(`ModelRuntime.${name}`);
		};
	}
	return runtime;
}

export function hostRuntime() {
	return runtimeFor(0);
}

export class ModelRuntime {
	static async create(options = {}) {
		for (const key of ["credentials", "modelsStore"]) {
			if (options[key] !== undefined) throw unsupportedError(`ModelRuntime.create.${key}`);
		}
		if (options.modelsPath === null) throw unsupportedError("ModelRuntime.create.modelsPath");
		const { signal, ...rest } = options;
		return runtimeFor(await callAsync("modelRuntime.create", { ...rest, signal: signalId(signal) }));
	}
}

/** Upstream `ModelRegistry`: the extension-facing registry over a runtime. */
export class ModelRegistry {
	constructor(runtime) {
		if (typeof runtime?._sdkRuntimeId !== "number") throw unsupportedError("new ModelRegistry");
		return registryFor(runtime._sdkRuntimeId);
	}
}
