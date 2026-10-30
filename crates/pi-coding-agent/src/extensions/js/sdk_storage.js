// Upstream `SessionManager` and `SettingsManager` for SDK use: each instance holds `_sdkId`, the id
// of a Rust manager in `ops_sdk_storage.rs`. Child sessions take the manager by that id.
import { call, optional } from "./bridge.js";
import { unsupportedError } from "./unsupported.js";

const sessionOp = (name, ...args) => optional(call(`storage.session.${name}`, ...args));
const settingsOp = (name, ...args) => optional(call(`storage.settings.${name}`, ...args));
const orNull = (value) => value ?? null;

function sessionInfo(info) {
	return {
		...info,
		name: optional(info.name),
		parentSessionPath: optional(info.parentSessionPath),
		created: new Date(info.created),
		modified: new Date(info.modified),
	};
}

// Reads with the default upstream returns for a missing value.
const SESSION_READS = {
	getCwd: undefined,
	getSessionDir: undefined,
	getSessionId: undefined,
	getSessionFile: undefined,
	getLeafEntry: undefined,
	getEntry: undefined,
	getLabel: undefined,
	buildSessionProjection: undefined,
	buildSessionContext: undefined,
	getSessionName: undefined,
	getLeafId: null,
	getHeader: null,
	buildContextEntries: [],
	getEntries: [],
	getTree: [],
};

export class SessionManager {
	constructor(id) {
		if (typeof id !== "number") throw unsupportedError("new SessionManager");
		this._sdkId = id;
	}

	_read(method, ...args) {
		return sessionOp("read", this._sdkId, method, ...args);
	}

	_write(method, ...args) {
		return sessionOp("write", this._sdkId, method, ...args);
	}

	/** `{ id, sessionId, cwd, sessionDir, sessionFile, inMemory, persisted, usesDefaultSessionDir }`. */
	_sdkDescriptor() {
		return sessionOp("descriptor", this._sdkId);
	}

	_sdkDispose() {
		sessionOp("dispose", this._sdkId);
	}

	static create(cwd, sessionDir, options) {
		return new SessionManager(sessionOp("create", orNull(cwd), orNull(sessionDir), options ?? {}));
	}

	static open(path, sessionDir, cwdOverride) {
		return new SessionManager(sessionOp("open", path, orNull(sessionDir), orNull(cwdOverride)));
	}

	static continueRecent(cwd, sessionDir) {
		return new SessionManager(sessionOp("continueRecent", orNull(cwd), orNull(sessionDir)));
	}

	static inMemory(cwd = process.cwd(), options, entries) {
		return new SessionManager(sessionOp("inMemory", cwd, options ?? {}, entries ?? []));
	}

	static forkFrom(sourcePath, targetCwd, sessionDir, options) {
		return new SessionManager(sessionOp("forkFrom", sourcePath, orNull(targetCwd), orNull(sessionDir), options ?? {}));
	}

	static findById(cwd, id, sessionDir) {
		return sessionOp("findById", cwd, id, orNull(sessionDir));
	}

	static async list(cwd, sessionDir, onProgress) {
		const sessions = sessionOp("list", cwd, orNull(sessionDir)).map(sessionInfo);
		onProgress?.(sessions.length, sessions.length, sessions);
		return sessions;
	}

	static async listAll(sessionDirOrOnProgress, onProgressOrSignal) {
		const sessionDir = typeof sessionDirOrOnProgress === "string" ? sessionDirOrOnProgress : null;
		const onProgress = typeof sessionDirOrOnProgress === "function" ? sessionDirOrOnProgress : onProgressOrSignal;
		const sessions = sessionOp("listAll", sessionDir).map(sessionInfo);
		if (typeof onProgress === "function") onProgress(sessions.length, sessions.length, sessions);
		return sessions;
	}

	isPersisted() {
		return this._sdkDescriptor().persisted;
	}

	usesDefaultSessionDir() {
		return this._sdkDescriptor().usesDefaultSessionDir;
	}

	getBranch(fromId) {
		return this._read("getBranch", fromId ?? null) ?? [];
	}

	getChildren(parentId) {
		return this.getEntries().filter((entry) => entry.parentId === parentId);
	}

	getEntryCount() {
		return this.getEntries().length;
	}

	setSessionFile(sessionFile) {
		this._write("setSessionFile", sessionFile);
	}

	newSession(options) {
		return this._write("newSession", options ?? {});
	}

	appendMessage(message) {
		return this._write("appendMessage", message);
	}

	appendThinkingLevelChange(thinkingLevel) {
		return this._write("appendThinkingLevelChange", thinkingLevel);
	}

	appendModelChange(provider, modelId) {
		return this._write("appendModelChange", provider, modelId);
	}

	appendUsage() {
		throw unsupportedError("SessionManager.appendUsage");
	}

	appendCompaction(summary, firstKeptEntryId, tokensBefore, details, fromHook, usage) {
		const rest = [details, fromHook, usage].map(orNull);
		return this._write("appendCompaction", summary, orNull(firstKeptEntryId), tokensBefore, ...rest);
	}

	appendCustomEntry(customType, data) {
		return this._write("appendCustomEntry", customType, orNull(data));
	}

	appendSessionInfo(name) {
		return this._write("appendSessionInfo", name);
	}

	appendCustomMessageEntry(customType, content, display, details) {
		return this._write("appendCustomMessageEntry", customType, content, Boolean(display), orNull(details));
	}

	appendContextEdit(targetId, replacement) {
		return this._write("appendContextEdit", targetId, orNull(replacement));
	}

	appendLabelChange(targetId, label) {
		return this._write("appendLabelChange", targetId, orNull(label));
	}

	branch(branchFromId) {
		this._write("branch", branchFromId);
	}

	resetLeaf() {
		this._write("resetLeaf");
	}

	branchWithSummary(branchFromId, summary, details, fromHook, usage) {
		const rest = [details, fromHook, usage].map(orNull);
		return this._write("branchWithSummary", orNull(branchFromId), summary, ...rest);
	}

	createBranchedSession(leafId) {
		return this._write("createBranchedSession", leafId);
	}
}

for (const [name, fallback] of Object.entries(SESSION_READS)) {
	SessionManager.prototype[name] = function (...args) {
		return this._read(name, ...args) ?? fallback;
	};
}

const SETTINGS_ACCESSORS = [
	"getLastChangelogVersion", "setLastChangelogVersion", "getSessionDir", "getDefaultProvider", "getDefaultModel",
	"setDefaultProvider", "setDefaultModel", "setDefaultModelAndProvider", "getThemeSetting", "getTheme", "setTheme",
	"getDefaultThinkingLevel", "setDefaultThinkingLevel", "getModelThinkingLevel", "getAllModelThinkingLevels",
	"setModelThinkingLevel", "removeModelThinkingLevel", "getTransport", "getCompactionEnabled", "setCompactionEnabled",
	"getCompactionReserveTokens", "getCompactionKeepRecentTokens", "getRetryEnabled", "setRetryEnabled",
	"getHideThinkingBlock", "setHideThinkingBlock", "getSteeringMode", "getFollowUpMode", "getExternalEditorCommand",
	"getShellPath", "setShellPath", "getShellCommandPrefix", "setShellCommandPrefix", "getDefaultProjectTrust",
	"getNpmCommand", "getEnableSkillCommands", "getEnabledModels", "setEnabledModels", "getDefaultTools",
	"getImageWidthCells", "setImageWidthCells", "getClearOnShrink", "setClearOnShrink", "getEnableAnalytics",
	"getHttpIdleTimeoutMs", "getExtensionPaths", "setExtensionPaths", "setProjectExtensionPaths", "getSkillPaths",
	"setSkillPaths", "setProjectSkillPaths", "getPromptTemplatePaths", "setPromptTemplatePaths",
	"setProjectPromptTemplatePaths", "getThemePaths", "setThemePaths", "setProjectThemePaths",
];

export class SettingsManager {
	constructor(id) {
		if (typeof id !== "number") throw unsupportedError("new SettingsManager");
		this._sdkId = id;
		// Upstream accessors without a Rust counterpart throw `ERR_PI_UNSUPPORTED` naming the member.
		return new Proxy(this, {
			get(target, property, receiver) {
				if (property in target || typeof property !== "string" || !/^(get|set|remove)[A-Z]/.test(property)) {
					return Reflect.get(target, property, receiver);
				}
				return () => {
					throw unsupportedError(`SettingsManager.${property}`);
				};
			},
		});
	}

	static create(cwd, agentDir, options) {
		return new SettingsManager(settingsOp("create", orNull(cwd), orNull(agentDir), options ?? {}));
	}

	static inMemory(settings, options) {
		return new SettingsManager(settingsOp("inMemory", settings ?? {}, options ?? {}));
	}

	static fromStorage() {
		throw unsupportedError("SettingsManager.fromStorage");
	}

	_sdkDispose() {
		settingsOp("dispose", this._sdkId);
	}

	getSettings() {
		return settingsOp("get", this._sdkId, "merged");
	}

	getGlobalSettings() {
		return settingsOp("get", this._sdkId, "global");
	}

	getProjectSettings() {
		return settingsOp("get", this._sdkId, "project");
	}

	isProjectTrusted() {
		return Boolean(settingsOp("isProjectTrusted", this._sdkId));
	}

	setProjectTrusted(trusted) {
		settingsOp("setProjectTrusted", this._sdkId, Boolean(trusted));
	}

	applyOverrides(overrides) {
		settingsOp("applyOverrides", this._sdkId, overrides ?? {});
	}

	async reload() {
		settingsOp("reload", this._sdkId);
	}

	async flush() {}

	drainErrors() {
		return settingsOp("drainErrors", this._sdkId).map(({ scope, path, message }) => ({
			scope,
			...(path ? { path } : {}),
			error: new Error(message),
		}));
	}
}

for (const name of SETTINGS_ACCESSORS) {
	SettingsManager.prototype[name] = function (...args) {
		return settingsOp("call", this._sdkId, name, ...args.map(orNull));
	};
}
