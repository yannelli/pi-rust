// Upstream `DefaultPackageManager` for SDK use, over the Rust package manager (`packages.*` ops).
import { call, callAsync, optional, registerCallback, releaseCallback } from "./bridge.js";
import { unsupportedError } from "./unsupported.js";

export class DefaultPackageManager {
	#options;
	#progress;

	constructor(options) {
		const { cwd, agentDir, settingsManager, builtinExtensions } = options;
		if (typeof settingsManager?._sdkId !== "number") throw unsupportedError("DefaultPackageManager.settingsManager");
		this.#options = { cwd, agentDir, settingsManagerId: settingsManager._sdkId, builtinExtensions: builtinExtensions ?? [] };
	}

	async #run(op, callback, ...args) {
		const progress = this.#progress ? registerCallback(this.#progress) : undefined;
		const key = callback ? registerCallback(callback) : undefined;
		try {
			const options = { ...this.#options, progress };
			return optional(await callAsync(`packages.${op}`, options, ...(op === "resolve" ? [key] : args)));
		} finally {
			if (progress !== undefined) releaseCallback(progress);
			if (key !== undefined) releaseCallback(key);
		}
	}

	setProgressCallback(callback) {
		this.#progress = callback;
	}

	resolve(onMissing) {
		return this.#run("resolve", onMissing);
	}

	resolveExtensionSources(sources, options) {
		return this.#run("resolveExtensionSources", undefined, sources, options ?? {});
	}

	async install(source, options) {
		await this.#run("install", undefined, source, Boolean(options?.local));
	}

	async installAndPersist(source, options) {
		await this.#run("installAndPersist", undefined, source, Boolean(options?.local));
	}

	async remove(source, options) {
		await this.#run("remove", undefined, source, Boolean(options?.local));
	}

	removeAndPersist(source, options) {
		return this.#run("removeAndPersist", undefined, source, Boolean(options?.local));
	}

	async update(source) {
		await this.#run("update", undefined, source ?? null);
	}

	checkForAvailableUpdates() {
		return this.#run("checkForAvailableUpdates");
	}

	listConfiguredPackages() {
		return call("packages.list", this.#options);
	}

	getInstalledPath(source, scope) {
		return optional(call("packages.installedPath", this.#options, source, scope));
	}

	addSourceToSettings(source, options) {
		return call("packages.addSource", this.#options, source, Boolean(options?.local));
	}

	removeSourceFromSettings(source, options) {
		return call("packages.removeSource", this.#options, source, Boolean(options?.local));
	}

	/** Private upstream, and read by extensions at runtime. */
	parseSource(source) {
		return call("packages.parseSource", source);
	}
}
