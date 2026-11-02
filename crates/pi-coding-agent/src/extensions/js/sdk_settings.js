// Upstream `SettingsManager` for SDK use. Rust (`settings/sdk_*.rs`) holds the layers and runs every
// member; this wrapper runs the write queue and the `withLock` calls of a custom storage.
import { homedir } from "node:os";
import { call, optional, registerCallback, releaseCallback } from "./bridge.js";
import { unsupportedError } from "./unsupported.js";

const op = (name, ...args) => optional(call(`storage.settings.${name}`, ...args));
const UNDEFINED = "\u0000undefined";
const NUMBER = "\u0000number";
const ARRAY = "\u0000array";
const ENV_MEMBERS = new Set(["getExternalEditorCommand", "getClearOnShrink", "getShowHardwareCursor", "getSessionDir", "getShellPath"]);

// Members Rust runs; `flush`, `reload`, `setProjectTrusted` and `drainErrors` are below.
const MEMBERS = [
	"getSettings", "getGlobalSettings", "getProjectSettings", "isProjectTrusted", "applyOverrides",
	"getLastChangelogVersion", "setLastChangelogVersion", "getExtensionChangelogVersion", "setExtensionChangelogVersion",
	"getSessionDir", "getDefaultProvider", "getDefaultModel", "setDefaultProvider", "setDefaultModel",
	"setDefaultModelAndProvider", "getSteeringMode", "setSteeringMode", "getFollowUpMode", "setFollowUpMode",
	"getThemeSetting", "getTheme", "setTheme", "getDefaultThinkingLevel", "setDefaultThinkingLevel",
	"getModelThinkingLevel", "getAllModelThinkingLevels", "setModelThinkingLevel", "removeModelThinkingLevel",
	"getTransport", "setTransport", "getSessionShareDestination", "setSessionShareDestination", "getCompactionEnabled",
	"setCompactionEnabled", "getCompactionReserveTokens", "getCompactionKeepRecentTokens", "getCompactionSettings",
	"getBranchSummarySettings", "getBranchSummarySkipPrompt", "getRetryEnabled", "setRetryEnabled", "getRetrySettings",
	"getHttpIdleTimeoutMs", "setHttpIdleTimeoutMs", "getCacheWarmingMode", "setCacheWarmingMode",
	"getProviderRetrySettings", "getWebSocketConnectTimeoutMs", "getHideThinkingBlock", "getShowCacheMissNotices",
	"getExternalEditorCommand", "setHideThinkingBlock", "setShowCacheMissNotices", "getShellPath", "setShellPath",
	"getQuietStartup", "setQuietStartup", "getDefaultProjectTrust", "setDefaultProjectTrust", "getShellCommandPrefix",
	"setShellCommandPrefix", "getNpmCommand", "setNpmCommand", "getCollapseChangelog", "setCollapseChangelog",
	"getEnableInstallTelemetry", "setEnableInstallTelemetry", "getEnableAnalytics", "getTrackingId",
	"setEnableAnalytics", "getOrCreateDeviceId", "getPackages", "setPackages", "setProjectPackages",
	"getExtensionPaths", "setExtensionPaths", "setProjectExtensionPaths", "getSkillPaths", "setSkillPaths",
	"setProjectSkillPaths", "getPromptTemplatePaths", "setPromptTemplatePaths", "setProjectPromptTemplatePaths",
	"getThemePaths", "setThemePaths", "setProjectThemePaths", "getEnableSkillCommands", "setEnableSkillCommands",
	"getThinkingBudgets", "getTerminalCapabilityOverrides", "getShowImages", "setShowImages", "getImageWidthCells",
	"setImageWidthCells", "getClearOnShrink", "setClearOnShrink", "getShowTerminalProgress", "setShowTerminalProgress",
	"getTuiMode", "setTuiMode", "getFullscreenExitOutput", "setFullscreenExitOutput", "getFullscreenScrollbar",
	"setFullscreenScrollbar", "getFullscreenCopyOnSelect", "setFullscreenCopyOnSelect", "getFullscreenWheelScrollLines",
	"setFullscreenWheelScrollLines", "getImageAutoResize", "setImageAutoResize", "getBlockImages", "setBlockImages",
	"getEnabledModels", "getDefaultTools", "setEnabledModels", "getDoubleEscapeAction", "setDoubleEscapeAction",
	"getTreeFilterMode", "setTreeFilterMode", "getShowHardwareCursor", "setShowHardwareCursor", "getEditorPaddingX",
	"setEditorPaddingX", "getOutputPad", "setOutputPad", "getAutocompleteMaxVisible", "setAutocompleteMaxVisible",
	"getCodeBlockIndent", "getMermaidRenderingMode", "setMermaidRenderingMode", "getWarnings", "setWarnings",
];

/** JSON for Rust that keeps `undefined` and non-finite numbers as sentinel objects. */
function encode(value) {
	if (value === undefined || typeof value === "function" || typeof value === "symbol") return { [UNDEFINED]: true };
	if (typeof value === "number") return Number.isFinite(value) ? value || 0 : { [NUMBER]: String(value) };
	if (Array.isArray(value)) return value.map(encode);
	if (value === null || typeof value !== "object") return value;
	return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, encode(item)]));
}

function decode(value) {
	if (Array.isArray(value)) return value.map(decode);
	if (value === null || typeof value !== "object") return value;
	const keys = Object.keys(value);
	if (keys.length === 1 && keys[0] === UNDEFINED) return undefined;
	if (keys.length === 1 && keys[0] === NUMBER) return Number(value[NUMBER]);
	const out = keys.includes(ARRAY) ? [] : {};
	for (const key of keys) if (key !== ARRAY) Object.defineProperty(out, key, { value: decode(value[key]), writable: true, enumerable: true, configurable: true });
	return out;
}

/** Replace the contents of `target` with those of `value`. */
function refill(target, value) {
	for (const key of Object.keys(target)) delete target[key];
	if (Array.isArray(target)) target.length = 0;
	Object.assign(target, value);
}

// Errors a storage threw, by token, so `drainErrors` returns the same objects upstream does.
const thrown = new Map();
let nextToken = 1;

function keep(error) {
	const token = nextToken++;
	thrown.set(token, error instanceof Error ? error : new Error(String(error)));
	return { token, message: thrown.get(token).message };
}

/** Upstream `loadFromStorage`'s read: the text, or the error `withLock` threw. */
function read(storage, scope) {
	let content;
	try {
		storage.withLock(scope, (current) => {
			content = current;
			return undefined;
		});
	} catch (error) {
		return { thrown: keep(error) };
	}
	return { content: encode(content) };
}

const environment = (...keys) => Object.fromEntries([...keys.map((key) => [key, process.env[key]]), ["HOME", homedir()]]);

/** Upstream `InMemorySettingsStorage`. */
export class InMemorySettingsStorage {
	withLock(scope, fn) {
		const current = scope === "global" ? this.global : this.project;
		const next = fn(current);
		if (next !== undefined) {
			if (scope === "global") this.global = next;
			else this.project = next;
		}
	}
}

export class SettingsManager {
	constructor(id, storage) {
		if (typeof id !== "number") throw unsupportedError("new SettingsManager");
		this._sdkId = id;
		this._storage = storage;
		this._queue = Promise.resolve();
		this._writing = "global";
		this._aliases = new Map();
		this._drainKey = registerCallback(() => this._claim());
		op("watch", id, this._drainKey);
	}

	static create(cwd, agentDir, options = {}) {
		const env = environment("PI_CODING_AGENT_DIR");
		return new SettingsManager(op("create", cwd ?? null, agentDir ?? null, options ?? {}, env));
	}

	static fromStorage(storage, options = {}) {
		const trusted = options.projectTrusted ?? true;
		const global = read(storage, "global");
		const project = trusted ? read(storage, "project") : null;
		return new SettingsManager(op("fromStorage", encode(options), global, project), storage);
	}

	static inMemory(settings = {}, options = {}) {
		return new SettingsManager(op("inMemory", encode(settings), encode(options)));
	}

	_sdkDispose() {
		releaseCallback(this._drainKey);
		op("dispose", this._sdkId);
	}

	/** Upstream `enqueueWrite` for each snapshot Rust queued. */
	_enqueue(count) {
		for (let index = 0; index < count; index++) {
			this._queue = this._queue.then(() => this._persistNext()).catch((error) => this._record(this._writing, error));
		}
	}

	_claim() {
		this._enqueue(op("claim", this._sdkId));
	}

	_persistNext() {
		const id = this._sdkId;
		if (!this._storage) return void op("persist", id);
		const next = op("persistTake", id);
		if (!next) return;
		this._writing = next.scope;
		if (next.error) throw new Error(next.error);
		this._storage.withLock(next.scope, (current) => op("persistText", id, encode(current)));
		op("persistDone", id, next.scope);
	}

	/** A stored object returned as is: changes through it write back to Rust, as they reach the
	 * settings upstream. */
	_live(token, value) {
		const key = JSON.stringify(token);
		const known = this._aliases.get(key);
		if (known) {
			refill(known.target, value);
			return known.proxy;
		}
		const sync = () => call("storage.settings.alias", this._sdkId, token, encode(value));
		const wrap = (target) => new Proxy(target, {
			get(object, property, receiver) {
				const item = Reflect.get(object, property, receiver);
				return item !== null && typeof item === "object" ? wrap(item) : item;
			},
			set(object, property, item) {
				const done = Reflect.set(object, property, item);
				sync();
				return done;
			},
			deleteProperty(object, property) {
				const done = Reflect.deleteProperty(object, property);
				sync();
				return done;
			},
			defineProperty(object, property, descriptor) {
				const done = Reflect.defineProperty(object, property, descriptor);
				sync();
				return done;
			},
		});
		const proxy = wrap(value);
		this._aliases.set(key, { target: value, proxy });
		return proxy;
	}

	_record(scope, error) {
		const { token, message } = keep(error);
		try {
			op("record", this._sdkId, scope, token, message);
		} catch {
			thrown.delete(token);
		}
	}

	setProjectTrusted(trusted) {
		const load = this._storage && trusted && this.isProjectTrusted() !== trusted;
		op("setProjectTrusted", this._sdkId, trusted, load ? read(this._storage, "project") : null);
	}

	async reload() {
		this._claim();
		await this._queue;
		if (!this._storage) return void op("reload", this._sdkId, null, null);
		const global = read(this._storage, "global");
		const project = this.isProjectTrusted() ? read(this._storage, "project") : null;
		op("reload", this._sdkId, global, project);
	}

	async flush() {
		this._claim();
		await this._queue;
	}

	drainErrors() {
		return op("drainErrors", this._sdkId).map(({ scope, path, message, token }) => {
			const error = thrown.get(token) ?? new Error(message);
			thrown.delete(token);
			return { scope, ...(path ? { path } : {}), error };
		});
	}
}

for (const name of MEMBERS) {
	SettingsManager.prototype[name] = function (...args) {
		const env = ENV_MEMBERS.has(name) ? environment("VISUAL", "EDITOR", "PI_CLEAR_ON_SHRINK", "PI_HARDWARE_CURSOR") : null;
		const { value, queued, alias } = call("storage.settings.call", this._sdkId, name, args.map(encode), env);
		this._enqueue(queued);
		return alias ? this._live(alias, decode(value)) : decode(value);
	};
}
