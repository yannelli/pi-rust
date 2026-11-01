// Upstream `DefaultResourceLoader`, `ModelRegistry`, and `ModelRuntime` for SDK use. Rust holds each
// loader's paths and layers (`resources.*` ops); this file runs the override hooks and keeps their objects.
import { homedir } from "node:os";
import { createEventBus } from "./api.js";
import { call, optional } from "./bridge.js";
import { createModelRegistry } from "./context.js";
import { bindExtensions, loadExtensions } from "./sdk_extensions.js";
import { SettingsManager } from "./sdk_storage.js";
import { sessionStream } from "./stream.js";
import { Theme } from "./theme.js";
import { unsupportedError } from "./unsupported.js";

const FIELDS = {
	skills: ["name", "description", "filePath", "baseDir", "sourceInfo", "disableModelInvocation", "license",
		"compatibility", "metadata", "allowedTools"],
	prompts: ["name", "description", "argumentHint", "content", "sourceInfo", "filePath"],
	themes: ["name", "sourcePath", "sourceInfo"],
};
const DIAGNOSTIC_FIELDS = ["type", "message", "path", "collision"];
const DIAGNOSTICS = { skills: "skillDiagnostics", prompts: "promptDiagnostics", themes: "themeDiagnostics" };

const res = (op, ...args) => call(`resources.${op}`, ...args);

const picker = (fields) => (item) => {
	const picked = {};
	for (const field of fields) {
		if (item?.[field] !== undefined) picked[field] = item[field];
	}
	return picked;
};

/** One kind of upstream `reload()`: Rust loads the base, the override runs here, Rust stores what it returned. */
function stage(loader, kind, report) {
	const id = loader._sdkId;
	const override = loader[`${kind}Override`];
	const base = res("load", id, kind);
	if (kind === "systemPrompt") {
		loader.systemPrompt = override ? override(optional(base.value)) : optional(base.value);
		loader.systemPromptSourcePath = optional(res("set", id, kind, loader.systemPrompt ?? null, false, base.inputs));
	} else if (kind === "appendSystemPrompt") {
		loader.appendSystemPrompt = override ? override(base.value) : base.value;
		loader.appendSystemPromptSourcePaths = res("set", id, kind, loader.appendSystemPrompt, false, base.inputs);
	} else if (kind === "agentsFiles") {
		const result = override ? override(base) : base;
		res("set", id, kind, result.agentsFiles.map(picker(["path", "content"])), false);
		loader.agentsFiles = result.agentsFiles;
	} else {
		if (kind === "themes") base.themes = base.themes.map((snapshot) => new Theme(snapshot));
		const result = override ? override(base) : base;
		const items = result[kind].map(picker(FIELDS[kind]));
		const payload = { [kind]: items, diagnostics: result.diagnostics.map(picker(DIAGNOSTIC_FIELDS)) };
		const { sourceInfos, missing } = res("set", id, kind, payload, report);
		loader[kind] = result[kind].map((item, index) => {
			const sourceInfo = sourceInfos[index] ?? item.sourceInfo;
			if (kind !== "themes") return { ...item, sourceInfo };
			item.sourceInfo = sourceInfo;
			return item;
		});
		loader[DIAGNOSTICS[kind]] = result.diagnostics;
		result.diagnostics.push(...missing);
	}
}

// While stages run, a nested `_sdkOptions()` reads the current values, as upstream getters do.
function stages(loader, kinds, report) {
	const state = loader._sdkState;
	state.staging += 1;
	try {
		for (const kind of kinds) stage(loader, kind, report);
	} finally {
		state.staging -= 1;
	}
}

export class DefaultResourceLoader {
	constructor(options) {
		if (options.extensionFactories?.some((input) => typeof input !== "function" && input.builtin === true)) {
			throw unsupportedError("DefaultResourceLoader.extensionFactories builtin entries");
		}
		if (options.settingsManager != null && typeof options.settingsManager._sdkId !== "number") {
			throw unsupportedError("DefaultResourceLoader.settingsManager");
		}
		const created = res("create", options.cwd, options.agentDir, homedir(), process.cwd());
		this._sdkId = created.id;
		this._sdkState = { prompts: false, staging: 0 };
		this._sdkExtensions = {};
		this.cwd = created.cwd;
		this.agentDir = created.agentDir;
		this.settingsManager = options.settingsManager ?? SettingsManager.create(this.cwd, this.agentDir);
		this.eventBus = options.eventBus ?? createEventBus();
		this.extensionFactories = options.extensionFactories ?? [];
		this.extensionsOverride = options.extensionsOverride;
		this.additionalExtensionPaths = options.additionalExtensionPaths ?? [];
		this.additionalSkillPaths = options.additionalSkillPaths ?? [];
		this.additionalPromptTemplatePaths = options.additionalPromptTemplatePaths ?? [];
		this.additionalThemePaths = options.additionalThemePaths ?? [];
		this.noExtensions = options.noExtensions ?? false;
		this.disabledBuiltinExtensions = new Set(options.disabledBuiltinExtensions);
		this.noSkills = options.noSkills ?? false;
		this.noPromptTemplates = options.noPromptTemplates ?? false;
		this.noThemes = options.noThemes ?? false;
		this.noContextFiles = options.noContextFiles ?? false;
		this.systemPromptSource = options.systemPrompt;
		this.appendSystemPromptSource = options.appendSystemPrompt;
		this.skillsOverride = options.skillsOverride;
		this.promptsOverride = options.promptsOverride;
		this.themesOverride = options.themesOverride;
		this.agentsFilesOverride = options.agentsFilesOverride;
		this.systemPromptOverride = options.systemPromptOverride;
		this.appendSystemPromptOverride = options.appendSystemPromptOverride;
		this.extensionsResult = { extensions: [], errors: [], runtime: undefined };
		this.skills = [];
		this.skillDiagnostics = [];
		this.prompts = [];
		this.promptDiagnostics = [];
		this.themes = [];
		this.themeDiagnostics = [];
		this.agentsFiles = [];
		this.systemPrompt = undefined;
		this.systemPromptSourcePath = undefined;
		this.appendSystemPrompt = [];
		this.appendSystemPromptSourcePaths = [];
		this.loaded = false;
		this._sdkConfigure();
	}

	/** Sends the settings manager and the current option values; upstream reads them live on each reload. */
	_sdkConfigure() {
		res("configure", this._sdkId, this.settingsManager._sdkId, {
			additionalExtensionPaths: this.additionalExtensionPaths,
			additionalSkillPaths: this.additionalSkillPaths,
			additionalPromptTemplatePaths: this.additionalPromptTemplatePaths,
			additionalThemePaths: this.additionalThemePaths,
			noExtensions: Boolean(this.noExtensions),
			disabledBuiltinExtensions: [...this.disabledBuiltinExtensions],
			noSkills: Boolean(this.noSkills),
			noPromptTemplates: Boolean(this.noPromptTemplates),
			noThemes: Boolean(this.noThemes),
			noContextFiles: Boolean(this.noContextFiles),
			systemPrompt: this.systemPromptSource ?? null,
			appendSystemPrompt: this.appendSystemPromptSource ?? null,
		});
	}

	getExtensions() {
		return this.extensionsResult;
	}

	getSkills() {
		return { skills: this.skills, diagnostics: this.skillDiagnostics };
	}

	getPrompts() {
		return { prompts: this.prompts, diagnostics: this.promptDiagnostics };
	}

	getThemes() {
		return { themes: this.themes, diagnostics: this.themeDiagnostics };
	}

	getAgentsFiles() {
		return { agentsFiles: this.agentsFiles };
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
		const kinds = res("extend", this._sdkId, {
			skillPaths: paths.skillPaths ?? [],
			promptPaths: paths.promptPaths ?? [],
			themePaths: paths.themePaths ?? [],
		});
		stages(this, kinds, false);
	}

	/** Upstream: user, global, and CLI extensions under an untrusted project, with the factories. */
	async loadProjectTrustExtensions() {
		this.settingsManager.setProjectTrusted(false);
		await this.settingsManager.reload();
		this._sdkConfigure();
		return (await loadExtensions(this, "pretrust")).result;
	}

	async reload(options) {
		const resolve = options?.resolveProjectTrust;
		if (resolve) {
			const extensionsResult = await this.loadProjectTrustExtensions();
			this.settingsManager.setProjectTrusted(await resolve({ extensionsResult }));
		}
		await this.settingsManager.reload();
		this._sdkConfigure();
		this.extensionsResult = (await loadExtensions(this, resolve ? "final" : "full")).result;
		stages(this, ["skills", "prompts", "themes"], true);
		stages(this, ["agentsFiles", "systemPrompt", "appendSystemPrompt"], false);
		this.loaded = true;
	}

	/** The options a child session reads; it shares this loader. Before `reload()` it loads the system prompts once. */
	_sdkOptions() {
		const state = this._sdkState;
		if (!this.loaded && !state.prompts && state.staging === 0) {
			this._sdkConfigure();
			stages(this, ["systemPrompt", "appendSystemPrompt"], false);
			state.prompts = true;
		}
		return {
			resourceLoaderId: this._sdkId,
			cwd: this.cwd,
			agentDir: this.agentDir,
			systemPrompt: this.systemPrompt,
			appendSystemPrompt: this.appendSystemPrompt,
			noExtensions: Boolean(this.noExtensions),
			noSkills: Boolean(this.noSkills),
			noPromptTemplates: Boolean(this.noPromptTemplates),
			noThemes: Boolean(this.noThemes),
			noContextFiles: Boolean(this.noContextFiles),
			additionalSkillPaths: this.additionalSkillPaths,
			additionalPromptTemplatePaths: this.additionalPromptTemplatePaths,
			settingsManagerId: this.settingsManager._sdkId,
		};
	}

	/** The child session `target` took the extensions of the last reload. */
	_sdkBind(target) {
		return bindExtensions(this, target);
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
