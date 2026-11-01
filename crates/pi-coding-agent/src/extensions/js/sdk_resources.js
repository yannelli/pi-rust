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

const RUNTIME_UNSUPPORTED = [
	"setRuntimeApiKey", "removeRuntimeApiKey", "listCredentials", "login", "logout",
	"registerProvider", "unregisterProvider", "registerNativeProvider", "registerVirtualModel",
	"unregisterVirtualModel", "getRegisteredProviderConfig", "getRegisteredNativeProvider",
	"getCompatibilityRequestConfig", "getPhysicalModel",
];

function rejectOptions(options, member) {
	for (const [key, value] of Object.entries(options ?? {})) {
		if (value !== undefined) throw unsupportedError(`${member}.${key}`);
	}
}

const ofProvider = (models, providerId) =>
	providerId === undefined ? models : models.filter((model) => model.provider === providerId);

/** Upstream `ModelRuntime` over the host session's models; auth methods take provider ids. */
export function hostRuntime() {
	const registry = createModelRegistry(0, sessionStream);
	const runtime = {
		...registry,
		_sdkHostRuntime: true,
		getModel: (providerId, modelId) => registry.find(providerId, modelId),
		getModels: (providerId) => registry.getModelsOfType("chat", providerId),
		getAllModels: (providerId) => ofProvider(registry.getAll(), providerId),
		getAllAvailable: async (providerId) => ofProvider(registry.getAvailable(), providerId),
		getAvailableSnapshot: () => registry.getAvailable(),
		getProviders: () =>
			[...new Set(registry.getAll().map((model) => model.provider))].map(registry.getProvider).filter(Boolean),
		hasConfiguredAuth: (providerId) => registry.hasConfiguredAuth({ provider: providerId }),
		isUsingOAuth: (providerId) => registry.isUsingOAuth({ provider: providerId }),
		isUsingSubscription: (providerId) => Boolean(call("models.read", 0, "isUsingSubscription", providerId)),
	};
	for (const name of RUNTIME_UNSUPPORTED) {
		runtime[name] = () => {
			throw unsupportedError(`ModelRuntime.${name}`);
		};
	}
	return runtime;
}

export class ModelRuntime {
	static async create(options) {
		rejectOptions(options, "ModelRuntime.create");
		return hostRuntime();
	}
}

/** Upstream `ModelRegistry`: the extension-facing registry over the host runtime. */
export class ModelRegistry {
	constructor(runtime) {
		if (!runtime?._sdkHostRuntime) throw unsupportedError("new ModelRegistry");
		return createModelRegistry(0, sessionStream);
	}
}
