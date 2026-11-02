// The `pi` object each extension factory receives (upstream `createExtensionAPI` and
// `createExtensionRuntime` in `loader.ts`). Registrations write to the extension; actions go to
// Rust once the runtime is bound, and throw while extensions are still loading.
import { constants as bufferConstants } from "node:buffer";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { StringDecoder } from "node:string_decoder";
import { call, callAsync } from "./bridge.js";
import { waitForChildProcess } from "./child_wait.js";

export function createEventBus() {
	const emitter = new EventEmitter();
	emitter.setMaxListeners(0);
	return {
		emit: (channel, data) => emitter.emit(channel, data),
		on: (channel, handler) => {
			const safe = async (data) => {
				try {
					await handler(data);
				} catch (error) {
					console.error(`Event handler error (${channel}):`, error);
				}
			};
			emitter.on(channel, safe);
			return () => emitter.off(channel, safe);
		},
	};
}

const DEFAULT_EXEC_MAX_BUFFER = Math.min(256 * 1024 * 1024, bufferConstants.MAX_STRING_LENGTH);
const FORCE_KILL_DELAY_MS = 5000;

// Run a command without a shell (upstream `execCommand`); pi-rust adds `options.stdin`. Output
// over `options.maxBuffer` stops the command with `truncated: true` (upstream issue 11360).
export function execCommand(command, args, cwd, options) {
	const maxBuffer = options?.maxBuffer ?? DEFAULT_EXEC_MAX_BUFFER;
	if (!Number.isInteger(maxBuffer) || maxBuffer < 0 || maxBuffer > bufferConstants.MAX_STRING_LENGTH) {
		return Promise.reject(new RangeError(`maxBuffer must be an integer between 0 and ${bufferConstants.MAX_STRING_LENGTH} bytes`));
	}
	return new Promise((resolve) => {
		const stdin = typeof options?.stdin === "string" ? options.stdin : undefined;
		const proc = spawn(command, args, { cwd, shell: false, stdio: [stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"] });
		if (stdin !== undefined) {
			proc.stdin?.on("error", () => undefined);
			proc.stdin?.end(stdin);
		}
		let stdout = "";
		let stderr = "";
		// A multi-byte character can be split between two chunks (upstream #11312).
		const stdoutDecoder = new StringDecoder("utf8");
		const stderrDecoder = new StringDecoder("utf8");
		let outputBytes = 0;
		let truncated = false;
		let killed = false;
		let timer;
		let forceKill;
		const kill = () => {
			if (killed) return;
			killed = true;
			proc.kill("SIGTERM");
			forceKill = setTimeout(() => {
				// `proc.killed` only says a signal was sent; check that the child has not exited.
				if (proc.exitCode === null && proc.signalCode === null) proc.kill("SIGKILL");
			}, FORCE_KILL_DELAY_MS);
		};
		if (options?.signal) {
			if (options.signal.aborted) kill();
			else options.signal.addEventListener("abort", kill, { once: true });
		}
		if (options?.timeout > 0) timer = setTimeout(kill, options.timeout);
		// Close both pipes on overflow, so a descendant that keeps writing cannot keep the call open.
		const take = (data) => {
			if (truncated) return undefined;
			const room = maxBuffer - outputBytes;
			if (data.length <= room) {
				outputBytes += data.length;
				return data;
			}
			truncated = true;
			proc.stdout?.destroy();
			proc.stderr?.destroy();
			kill();
			return room > 0 ? data.subarray(0, room) : undefined;
		};
		proc.stdout?.on("data", (data) => {
			const kept = take(data);
			if (kept) stdout += stdoutDecoder.write(kept);
		});
		proc.stderr?.on("data", (data) => {
			const kept = take(data);
			if (kept) stderr += stderrDecoder.write(kept);
		});
		let finished = false;
		const finish = (code, error) => {
			if (finished) return;
			finished = true;
			clearTimeout(timer);
			clearTimeout(forceKill);
			options?.signal?.removeEventListener("abort", kill);
			// A truncated result drops an incomplete character instead of adding U+FFFD.
			if (!truncated) {
				stdout += stdoutDecoder.end();
				stderr += stderrDecoder.end();
			}
			// A command that could not start reports why, unlike one that exits 1 without output.
			if (error && !stderr) stderr = error.message;
			const exitCode = code ?? 0;
			resolve({ stdout, stderr, code: truncated && exitCode === 0 ? 1 : exitCode, killed, truncated });
		};
		// Do not hang on inherited stdio handles held open by detached descendants.
		waitForChildProcess(proc).then((code) => finish(code), (error) => finish(1, error));
	});
}

/** Shared state of all extensions of one session. */
export function createRuntime(onChange) {
	const notBound = () => {
		throw new Error("Extension runtime not initialized. Action methods cannot be called during extension loading.");
	};
	const unsubscribers = new Set();
	const runtime = {
		bound: false,
		target: 0,
		modelsBound: false,
		staleMessage: undefined,
		flagValues: new Map(),
		pendingProviders: [],
		pendingVirtualModels: [],
		providers: new Map(),
		virtualModels: new Map(),
		modelCall: call,
		action: notBound,
		markChanged: onChange,
		assertActive() {
			if (runtime.staleMessage) throw new Error(runtime.staleMessage);
		},
		invalidate(message) {
			if (runtime.staleMessage) return;
			runtime.staleMessage = message;
			for (const unsubscribe of unsubscribers) unsubscribe();
			unsubscribers.clear();
		},
		track(unsubscribe) {
			let active = true;
			const tracked = () => {
				if (!active) return;
				active = false;
				unsubscribers.delete(tracked);
				unsubscribe();
			};
			unsubscribers.add(tracked);
			return tracked;
		},
		registerProvider(name, config, extensionPath) {
			runtime.providers.set(name, { config, extensionPath });
			if (!runtime.modelsBound) {
				runtime.pendingProviders.push(name);
				return;
			}
			runtime.applyProvider(name);
		},
		applyProvider(name) {
			const entry = runtime.providers.get(name);
			if (!entry) return;
			try {
				runtime.modelCall("provider.register", name, providerJson(entry.config), entry.extensionPath);
			} catch (error) {
				call("error", {
					extensionPath: entry.extensionPath,
					event: "register_provider",
					error: error instanceof Error ? error.message : String(error),
					stack: null,
				});
			}
		},
		unregisterProvider(name) {
			runtime.providers.delete(name);
			runtime.pendingProviders = runtime.pendingProviders.filter((pending) => pending !== name);
			if (runtime.modelsBound) runtime.modelCall("provider.unregister", name);
		},
		registerVirtualModel(definition, extensionPath) {
			const key = `${definition.provider}/${definition.id}`;
			runtime.virtualModels.set(key, { definition, extensionPath });
			if (!runtime.modelsBound) {
				runtime.pendingVirtualModels.push(key);
				return;
			}
			runtime.applyVirtualModel(key);
		},
		applyVirtualModel(key) {
			const entry = runtime.virtualModels.get(key);
			if (!entry) return;
			const { route: _route, ...definition } = entry.definition;
			try {
				runtime.modelCall("virtualModel.register", definition, entry.extensionPath);
			} catch (error) {
				call("error", {
					extensionPath: entry.extensionPath,
					event: "register_virtual_model",
					error: error instanceof Error ? error.message : String(error),
					stack: null,
				});
			}
		},
		unregisterVirtualModel(provider, id) {
			const key = `${provider}/${id}`;
			runtime.virtualModels.delete(key);
			runtime.pendingVirtualModels = runtime.pendingVirtualModels.filter((pending) => pending !== key);
			if (runtime.modelsBound) runtime.modelCall("virtualModel.unregister", provider, id);
		},
		/** The model runtime exists: apply queued providers and virtual models, then directly. */
		bindModels() {
			runtime.modelsBound = true;
			for (const name of runtime.pendingProviders.splice(0)) runtime.applyProvider(name);
			for (const key of runtime.pendingVirtualModels.splice(0)) runtime.applyVirtualModel(key);
		},
		/** Rust bound the session: actions go to it. */
		bind() {
			runtime.bindModels();
			runtime.bound = true;
			runtime.action = (op, ...args) => {
				runtime.assertActive();
				runtime.markChanged(true);
				return call(op, runtime.target, ...args);
			};
		},
	};
	return runtime;
}

/** Provider config without its functions; Rust calls those back by provider name. */
function providerJson(config) {
	const { streamSimple, oauth, headers, refreshModels, images, classifiers, ...rest } = config;
	const json = { ...rest, hasStreamSimple: typeof streamSimple === "function" };
	json.hasRefreshModels = typeof refreshModels === "function";
	json.imageApis = Object.keys(images ?? {});
	json.classifierApis = Object.keys(classifiers ?? {});
	if (headers !== undefined) json.headers = typeof headers === "function" ? undefined : headers;
	json.hasHeaderFunction = typeof headers === "function";
	if (oauth) {
		json.oauth = {
			name: oauth.name,
			builtin: typeof oauth.builtin === "string" ? oauth.builtin : undefined,
			hasModifyModels: typeof oauth.modifyModels === "function",
			usesCallbackServer: Boolean(oauth.usesCallbackServer),
		};
	}
	return json;
}

export function createExtension(path, resolvedPath, sourceInfo) {
	return {
		path,
		resolvedPath,
		sourceInfo,
		handlers: new Map(),
		tools: new Map(),
		messageRenderers: new Map(),
		entryRenderers: new Map(),
		toolRenderers: [],
		markdownTransformer: undefined,
		commands: new Map(),
		flags: new Map(),
		shortcuts: new Map(),
	};
}

/** The API object, plus `commit` and `discard` for the end of the factory call. */
export function createExtensionAPI(extension, runtime, cwd, eventBus) {
	const pendingFlags = new Map();
	const pendingChanges = [];
	const loadingUnsubscribers = [];
	let state = "loading";
	const assertActive = () => {
		if (state === "failed") throw new Error(`Extension "${extension.path}" failed to load and its API is no longer active.`);
		runtime.assertActive();
	};
	const apply = (change) => {
		if (state === "loading") pendingChanges.push(change);
		else change();
	};
	const changed = () => runtime.markChanged(false);
	const api = {
		on(event, handler) {
			assertActive();
			const registered = (...args) => handler(...args);
			const list = extension.handlers.get(event) ?? [];
			list.push(registered);
			extension.handlers.set(event, list);
			changed();
			return () => {
				const handlers = extension.handlers.get(event);
				const index = handlers?.indexOf(registered) ?? -1;
				if (index === -1) return;
				handlers.splice(index, 1);
				if (handlers.length === 0) extension.handlers.delete(event);
				changed();
			};
		},
		registerTool(tool) {
			assertActive();
			if (typeof tool.parameters !== "object" || tool.parameters === null || Array.isArray(tool.parameters)) {
				throw new Error(
					`Tool "${tool.name}" registered by extension "${extension.path}" must define an object parameter schema.`,
				);
			}
			extension.tools.set(tool.name, { definition: tool, sourceInfo: extension.sourceInfo });
			changed();
		},
		registerCommand(name, options) {
			assertActive();
			if (typeof name !== "string" || name.length === 0) {
				throw new Error(
					`Command registered by extension "${extension.path}" must have a non-empty string name. Use pi.registerCommand("name", { description, handler }).`,
				);
			}
			if (typeof options?.handler !== "function") {
				throw new Error(`Command "/${name}" registered by extension "${extension.path}" must define handler().`);
			}
			extension.commands.set(name, { name, sourceInfo: extension.sourceInfo, ...options });
			changed();
		},
		registerShortcut(shortcut, options) {
			assertActive();
			extension.shortcuts.set(shortcut, { shortcut, extensionPath: extension.path, ...options });
			changed();
		},
		registerFlag(name, options) {
			assertActive();
			if (options.default !== undefined && typeof options.default !== options.type) {
				throw new Error(`Invalid default for flag "${name}": expected ${options.type}, got ${typeof options.default}`);
			}
			extension.flags.set(name, { name, extensionPath: extension.path, ...options });
			if (options.default !== undefined && !runtime.flagValues.has(name)) {
				if (state === "loading") {
					if (!pendingFlags.has(name)) pendingFlags.set(name, options.default);
				} else {
					runtime.flagValues.set(name, options.default);
				}
			}
			changed();
		},
		registerMessageRenderer(customType, renderer) {
			assertActive();
			extension.messageRenderers.set(customType, renderer);
			changed();
		},
		registerMarkdownTransformer(transformer) {
			assertActive();
			extension.markdownTransformer = transformer;
			changed();
		},
		registerEntryRenderer(customType, renderer) {
			assertActive();
			extension.entryRenderers.set(customType, renderer);
			changed();
		},
		registerToolRenderer(resolver) {
			assertActive();
			extension.toolRenderers.push(resolver);
			changed();
		},
		getFlag(name) {
			assertActive();
			if (!extension.flags.has(name)) return undefined;
			return runtime.flagValues.has(name) ? runtime.flagValues.get(name) : pendingFlags.get(name);
		},
		sendMessage(message, options) {
			assertActive();
			runtime.action("action.sendMessage", message, options ?? {});
		},
		sendUserMessage(content, options) {
			assertActive();
			runtime.action("action.sendUserMessage", content, options ?? {});
		},
		appendEntry(customType, data) {
			assertActive();
			runtime.action("action.appendEntry", customType, data === undefined ? null : data);
		},
		setSessionName(name) {
			assertActive();
			runtime.action("action.setSessionName", name);
		},
		getSessionName() {
			assertActive();
			return runtime.action("action.getSessionName") ?? undefined;
		},
		setLabel(entryId, label) {
			assertActive();
			runtime.action("action.setLabel", entryId, label ?? null);
		},
		exec(command, args, options) {
			assertActive();
			return execCommand(command, args ?? [], options?.cwd ?? cwd, options);
		},
		getActiveTools() {
			assertActive();
			return runtime.action("action.getActiveTools");
		},
		getAllTools() {
			assertActive();
			return runtime.action("action.getAllTools").map((info) => {
				if (info.reachesTools !== true) return info;
				// A tool registered by an extension reports its own function; others ask Rust.
				const own = runtime.findToolDefinition?.(info.name)?.reachesTools;
				const reachesTools = typeof own === "function" ? own : (input) => runtime.action("action.toolReaches", info.name, input);
				return { ...info, reachesTools };
			});
		},
		getSettings() {
			assertActive();
			return runtime.action("action.getSettings");
		},
		setActiveTools(toolNames) {
			assertActive();
			runtime.action("action.setActiveTools", toolNames);
		},
		getCommands() {
			assertActive();
			return runtime.action("action.getCommands");
		},
		setModel(model) {
			assertActive();
			if (!runtime.bound) return Promise.reject(new Error("Extension runtime not initialized"));
			runtime.markChanged(true);
			return callAsync("action.setModel", runtime.target, model);
		},
		getThinkingLevel() {
			assertActive();
			return runtime.action("action.getThinkingLevel");
		},
		setThinkingLevel(level) {
			assertActive();
			runtime.action("action.setThinkingLevel", level);
		},
		registerProvider(providerOrName, config) {
			assertActive();
			if (typeof providerOrName !== "string") {
				const provider = providerOrName;
				if (!provider?.id) throw new Error("Provider config is required when registering by name");
				apply(() => runtime.registerProvider(provider.id, { ...provider, native: true }, extension.path));
				return;
			}
			if (!config) throw new Error("Provider config is required when registering by name");
			apply(() => runtime.registerProvider(providerOrName, config, extension.path));
		},
		unregisterProvider(name) {
			assertActive();
			apply(() => runtime.unregisterProvider(name));
		},
		registerMcpServer(name, config) {
			assertActive();
			const register = () => {
				const error = call("mcp.register", extension.path, name, config ?? null);
				if (error) throw new Error(error);
			};
			if (state !== "loading") return register();
			const invalid = call("mcp.check", extension.path, name, config ?? null);
			if (invalid) throw new Error(invalid);
			pendingChanges.push(register);
		},
		unregisterMcpServer(name) {
			assertActive();
			apply(() => call("mcp.unregister", name, extension.path));
		},
		getMcpServers() {
			assertActive();
			return call("mcp.list");
		},
		registerVirtualModel(model) {
			assertActive();
			apply(() => runtime.registerVirtualModel(model, extension.path));
		},
		unregisterVirtualModel(provider, id) {
			assertActive();
			apply(() => runtime.unregisterVirtualModel(provider, id));
		},
		events: {
			emit(channel, data) {
				assertActive();
				eventBus.emit(channel, data);
			},
			on(channel, handler) {
				assertActive();
				const unsubscribe = runtime.track(eventBus.on(channel, handler));
				if (state === "loading") loadingUnsubscribers.push(unsubscribe);
				return unsubscribe;
			},
		},
	};
	return {
		api,
		commit() {
			if (state !== "loading") return;
			runtime.assertActive();
			for (const [name, value] of pendingFlags) if (!runtime.flagValues.has(name)) runtime.flagValues.set(name, value);
			state = "active";
			for (const change of pendingChanges) {
				try {
					change();
				} catch (error) {
					call("error", { extensionPath: extension.path, event: "load", error: String(error?.message ?? error), stack: null });
				}
			}
		},
		discard() {
			if (state !== "loading") return;
			state = "failed";
			for (const unsubscribe of loadingUnsubscribers) unsubscribe();
		},
	};
}
