// Entry point Rust imports: loads extensions and exposes the runner, tools, commands, renderers,
// providers, and virtual models to the Rust extension host through `createHost()`.
import { createEventBus, createExtension, createExtensionAPI, createRuntime } from "./api.js";
import * as bridge from "./bridge.js";
import { call, callAsync, errorMessage, optional, registerCallback, releaseCallback, signalFor } from "./bridge.js";
import { Runner } from "./runner.js";
import { sessionStream } from "./stream.js";
import { currentTheme } from "./theme.js";
import * as ui from "./ui.js";

function toolDescriptor(definition, sourceInfo, extensionPath) {
	return {
		name: definition.name,
		label: definition.label ?? definition.name,
		description: definition.description ?? "",
		parameters: definition.parameters,
		promptSnippet: definition.promptSnippet,
		promptGuidelines: definition.promptGuidelines,
		constrainedSampling: definition.constrainedSampling,
		renderShell: definition.renderShell,
		outputSchema: definition.outputSchema,
		exposure: definition.exposure,
		namespace: definition.namespace,
		annotations: definition.annotations,
		defaultActive: definition.defaultActive,
		executionMode: definition.executionMode,
		hasPrepareArguments: typeof definition.prepareArguments === "function",
		hasPrepareLoadout: typeof definition.prepareLoadout === "function",
		hasRenderCall: typeof definition.renderCall === "function",
		hasRenderResult: typeof definition.renderResult === "function",
		sourceInfo,
		extensionPath,
	};
}

function normalizeToolResult(result) {
	if (!result || typeof result !== "object") return { content: [] };
	return { ...result, content: Array.isArray(result.content) ? result.content : [] };
}

export function createHost(config) {
	let dirty = false;
	let scheduled = false;
	const runtime = createRuntime((immediate) => {
		dirty = true;
		if (immediate) host.flush();
		else if (!scheduled) {
			scheduled = true;
			queueMicrotask(() => {
				scheduled = false;
				host.flush();
			});
		}
	});
	const runner = new Runner(runtime, sessionStream);
	const eventBus = createEventBus();

	const findTool = (name) => {
		for (const ext of runner.extensions) {
			const tool = ext.tools.get(name);
			if (tool) return tool.definition;
		}
		return undefined;
	};

	const resolveCommands = () => {
		const commands = runner.extensions.flatMap((ext) => [...ext.commands.values()]);
		const counts = new Map();
		for (const command of commands) counts.set(command.name, (counts.get(command.name) ?? 0) + 1);
		const seen = new Map();
		const taken = new Set();
		return commands.map((command) => {
			const occurrence = (seen.get(command.name) ?? 0) + 1;
			seen.set(command.name, occurrence);
			let invocationName = counts.get(command.name) > 1 ? `${command.name}:${occurrence}` : command.name;
			let suffix = occurrence;
			while (taken.has(invocationName)) invocationName = `${command.name}:${++suffix}`;
			taken.add(invocationName);
			return { ...command, invocationName };
		});
	};

	const firstPerName = (key) => {
		const all = new Map();
		for (const ext of runner.extensions) {
			for (const [name, value] of ext[key]) if (!all.has(name)) all.set(name, value);
		}
		return all;
	};

	const host = {
		runner,
		/** Load extension files in order; failures are collected, not thrown. */
		async load(entries) {
			const errors = [];
			for (const entry of entries) {
				try {
					const module = await import(entry.resolvedPath);
					const factory = typeof module.default === "function" ? module.default : module;
					if (typeof factory !== "function") {
						errors.push({ path: entry.path, error: `Extension does not export a valid factory function: ${entry.path}` });
						continue;
					}
					const extension = createExtension(entry.path, entry.resolvedPath, entry.sourceInfo);
					const load = createExtensionAPI(extension, runtime, config.cwd, eventBus);
					try {
						await factory(load.api);
						load.commit();
					} catch (error) {
						load.discard();
						throw error;
					}
					runner.extensions.push(extension);
				} catch (error) {
					errors.push({ path: entry.path, error: `Failed to load extension: ${errorMessage(error)}` });
				}
			}
			dirty = true;
			host.flush();
			return errors;
		},
		bind() {
			runtime.bind();
			dirty = true;
			host.flush();
		},
		/** Send the registry snapshot to Rust when registrations changed. */
		flush() {
			if (!dirty) return;
			dirty = false;
			const tools = [];
			const seenTools = new Set();
			for (const ext of runner.extensions) {
				for (const tool of ext.tools.values()) {
					if (seenTools.has(tool.definition.name)) continue;
					seenTools.add(tool.definition.name);
					tools.push(toolDescriptor(tool.definition, tool.sourceInfo, ext.path));
				}
			}
			const flags = [...firstPerName("flags").values()].map(({ name, description, type, extensionPath }) => ({
				name,
				description,
				type,
				default: runtime.flagValues.get(name),
				extensionPath,
			}));
			call("registry", {
				handlers: runner.handlerTypes(),
				tools,
				commands: resolveCommands().map((command) => ({
					name: command.name,
					invocationName: command.invocationName,
					description: command.description,
					sourceInfo: command.sourceInfo,
					hasCompletions: typeof command.getArgumentCompletions === "function",
				})),
				flags,
				shortcuts: runner.extensions.flatMap((ext) =>
					[...ext.shortcuts.values()].map(({ shortcut, description, extensionPath }) => ({ shortcut, description, extensionPath })),
				),
				messageRenderers: [...firstPerName("messageRenderers").keys()],
				entryRenderers: [...firstPerName("entryRenderers").keys()],
				toolRenderers: runner.extensions.reduce((count, ext) => count + ext.toolRenderers.length, 0),
				markdownTransformers: runner.extensions.filter((ext) => ext.markdownTransformer).length,
				extensions: runner.extensions.map((ext) => ({ path: ext.path, sourceInfo: ext.sourceInfo })),
			});
		},
		hasHandlers: (type) => runner.hasHandlers(type),
		emit: (event) => runner.emit(event),
		emitToolCall: (event) => runner.emitToolCall(event),
		emitToolResult: (event) => runner.emitToolResult(event),
		emitMessageEnd: (event) => runner.emitMessageEnd(event),
		emitContext: (messages) => runner.emitContext(messages),
		emitBeforeProviderRequest: (payload) => runner.emitBeforeProviderRequest(payload),
		emitBeforeProviderHeaders: (headers) => runner.emitBeforeProviderHeaders(headers),
		emitBeforeAgentStart: (prompt, images, options) => runner.emitBeforeAgentStart(prompt, optional(images), options),
		emitResourcesDiscover: (cwd, reason) => runner.emitResourcesDiscover(cwd, reason),
		emitInput: (text, images, source, behavior) => runner.emitInput(text, optional(images), source, optional(behavior)),
		emitCacheWarmingDecision: (event) => runner.emitCacheWarmingDecision(event),
		emitBoundary: (event) => runner.emitBoundary(event),
		async emitUserBash(event) {
			const result = await runner.emitUserBash(event);
			if (result?.operations) {
				const operations = result.operations;
				return { operations: registerCallback((command, cwd, options) => runOperations(operations, command, cwd, options)) };
			}
			return result;
		},
		async emitProjectTrust(cwd) {
			const ctx = runner.createContext();
			return runner.emitProjectTrust({ type: "project_trust", cwd }, ctx);
		},
		async executeTool(name, toolCallId, params, signal, updateKey) {
			const tool = findTool(name);
			if (!tool) throw new Error(`Tool ${name} is not registered`);
			const abort = signalFor(signal);
			const onUpdate = updateKey ? (partial) => call("tool.update", updateKey, normalizeToolResult(partial)) : undefined;
			const ctx = runner.createToolContext(toolCallId, abort);
			return normalizeToolResult(await tool.execute(toolCallId, params, abort, onUpdate, ctx));
		},
		prepareArguments: (name, args) => findTool(name)?.prepareArguments?.(args) ?? args,
		prepareLoadout(name, loadout) {
			const view = {
				...loadout,
				getExposure: (tool) => loadout.exposures?.[tool] ?? "direct",
				getNamespace: (tool) => loadout.namespaces?.[tool],
				getPromptGuidelines: (tool) => loadout.guidelines?.[tool] ?? [],
			};
			return findTool(name)?.prepareLoadout?.(view) ?? null;
		},
		async runCommand(name, args) {
			const command = resolveCommands().find((candidate) => candidate.invocationName === name);
			if (!command) return false;
			try {
				await command.handler(args, runner.createCommandContext());
			} catch (error) {
				runner.emitError({ extensionPath: `command:${name}`, event: "command", error: errorMessage(error) });
			}
			return true;
		},
		async commandCompletions(name, prefix) {
			const command = resolveCommands().find((candidate) => candidate.invocationName === name);
			const items = await command?.getArgumentCompletions?.(prefix);
			return Array.isArray(items) ? items : null;
		},
		async runShortcut(extensionPath, shortcut) {
			const ext = runner.extensions.find((candidate) => candidate.path === extensionPath);
			const entry = ext?.shortcuts.get(shortcut);
			if (!entry) return false;
			try {
				await entry.handler(runner.createContext());
			} catch (error) {
				runner.emitError({ extensionPath, event: "shortcut", error: errorMessage(error) });
			}
			return true;
		},
		setFlag(name, value) {
			runtime.flagValues.set(name, value);
		},
		invalidate: (message) => runner.invalidate(optional(message)),
		abortSignal: bridge.abortSignal,
		releaseSignal: bridge.releaseSignal,
		invokeCallback: (key, ...args) => bridge.invokeCallback(key, ...args),
		releaseCallback: (key) => releaseCallback(key),
		async providerStream(name, model, context, options, sink) {
			const provider = runtime.providers.get(name)?.config;
			if (!provider?.streamSimple) throw new Error(`Provider ${name} has no streamSimple`);
			const stream = provider.streamSimple(model, context, { ...options, signal: signalFor(options.signal) });
			for await (const event of stream) call("stream.event", sink, event);
			return optional(await stream.result?.());
		},
		async providerHeaders(name, model) {
			const headers = runtime.providers.get(name)?.config.headers;
			return typeof headers === "function" ? await headers(model) : headers;
		},
		async oauth(name, method, args) {
			const oauth = runtime.providers.get(name)?.config.oauth;
			if (!oauth?.[method]) throw new Error(`Provider ${name} has no OAuth ${method}`);
			if (method === "login") return await oauth.login(loginCallbacks(args[0]));
			return await oauth[method](...args);
		},
		async route(key, request) {
			const entry = runtime.virtualModels.get(key);
			if (!entry) throw new Error(`Virtual model ${key} is not registered`);
			const signal = signalFor(request.signal);
			return await entry.definition.route({ ...request, signal }, runner.createContext());
		},
		createComponent: ui.createComponent,
		terminalInput: ui.terminalInput,
		renderMessage(customType, message, options) {
			const renderer = firstPerName("messageRenderers").get(customType);
			return renderer ? (renderer(message, options, currentTheme()) ?? null) : null;
		},
		renderEntry(customType, entry, options) {
			const renderer = firstPerName("entryRenderers").get(customType);
			return renderer ? (renderer(entry, options, currentTheme()) ?? null) : null;
		},
		/** Renderers of calls to `toolName`: extension resolvers in load order, then the tool's own. */
		toolRenderers(toolName) {
			const resolvers = runner.extensions.flatMap((ext) => ext.toolRenderers);
			const base = () => {
				const tool = findTool(toolName);
				return tool ? { renderShell: tool.renderShell, renderCall: tool.renderCall, renderResult: tool.renderResult } : undefined;
			};
			const resolve = (index) => (index < resolvers.length ? resolvers[index](toolName, () => resolve(index + 1)) : base());
			return resolve(0) ?? null;
		},
		transformMarkdown(markdown, context) {
			let text = markdown;
			for (const ext of runner.extensions) {
				if (!ext.markdownTransformer) continue;
				try {
					text = ext.markdownTransformer(text, context) ?? text;
				} catch (error) {
					runner.emitError({ extensionPath: ext.path, event: "markdown_transform", error: errorMessage(error) });
				}
			}
			return text;
		},
		theme: () => currentTheme(),
	};
	return host;
}

async function runOperations(operations, command, cwd, options) {
	const onData = options.dataKey ? (data) => call("bash.data", options.dataKey, typeof data === "string" ? data : String(data)) : () => {};
	const result = await operations.exec(command, cwd, {
		onData,
		signal: signalFor(options.signal),
		timeout: options.timeout,
		env: options.env,
	});
	return { exitCode: result?.exitCode ?? null };
}

function loginCallbacks(keys) {
	const forward = (name) => (...args) => callAsync("oauth.callback", keys, name, ...args.map((arg) => (typeof arg === "function" ? null : arg)));
	return {
		onAuth: (info) => void forward("onAuth")(info),
		onPrompt: (prompt) => forward("onPrompt")(prompt),
		onProgress: (message) => void forward("onProgress")(message),
		onManualCodeInput: () => forward("onManualCodeInput")(),
		onSelect: (prompt) => forward("onSelect")(prompt),
		signal: signalFor(keys.signal),
	};
}
