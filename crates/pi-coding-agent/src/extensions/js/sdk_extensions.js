// Extensions of an SDK `DefaultResourceLoader`. Each `extensionFactories` entry runs in this host; its
// child host mirror (`sdk_mirror.js`) forwards calls here with the child session's target.
import { createExtension, createExtensionAPI, createRuntime } from "./api.js";
import { call, callAsync, signalFor } from "./bridge.js";
import { createCommandContext, createContext, createToolContext } from "./context.js";
import { Runner, STALE_MESSAGE } from "./runner.js";
import { busCall, closeBus, dataOf, openBus, plain } from "./sdk_mirror.js";
import { sessionStream } from "./stream.js";
import { unsupportedError } from "./unsupported.js";

const HOOKS = ["prepareArguments", "reachesTools", "prepareLoadout"];
const MCP = ["registerMcpServer", "unregisterMcpServer", "getMcpServers"];
const FILE_ONLY = ["handlers", "shortcuts", "messageRenderers", "entryRenderers", "toolRenderers"];
const groups = new Map();

/** The runtime and session context of factory group `id`, for its provider and virtual model callbacks. */
export function groupScope(id) {
	const group = groups.get(id);
	if (!group) throw new Error(STALE_MESSAGE);
	const context = () => {
		if (!group.runtime.bound) throw new Error("Extension runtime not initialized");
		return createContext(group.runner, group.runtime.target);
	};
	return [group.runtime, context];
}

const normalize = (result) =>
	result && typeof result === "object" ? { ...result, content: Array.isArray(result.content) ? result.content : [] } : { content: [] };

function descriptor(group, extension) {
	const id = (handler) => {
		if (!group.ids.has(handler)) group.ids.set(handler, group.nextId++);
		return group.ids.get(handler);
	};
	return {
		path: extension.path,
		handlers: [...extension.handlers].flatMap(([type, list]) => list.map((handler) => [type, id(handler)])),
		tools: [...extension.tools.values()].map(({ definition }) => ({
			definition: dataOf(definition),
			hooks: Object.fromEntries(HOOKS.map((name) => [name, typeof definition[name] === "function"])),
		})),
		commands: [...extension.commands.values()].map((command) => ({
			name: command.name,
			description: command.description,
			completions: typeof command.getArgumentCompletions === "function",
		})),
	};
}

function publish(group) {
	group.scheduled = false;
	if (groups.get(group.id) !== group) return;
	call("resources.mirror", group.loaderId, group.id, group.extensions.map((extension) => descriptor(group, extension)));
}

function createGroup(id, loader) {
	const group = { id, loaderId: loader._sdkId, loader, ids: new WeakMap(), nextId: 1 };
	group.runtime = createRuntime((immediate) => {
		if (!group.published) return;
		if (immediate) {
			if (group.scheduled) publish(group);
		} else if (!group.scheduled) {
			group.scheduled = true;
			queueMicrotask(() => group.scheduled && publish(group));
		}
	});
	group.runtime.modelCall = (op, ...args) => call("resources.models", id, op, ...args);
	group.runner = new Runner(group.runtime, sessionStream);
	group.extensions = group.runner.extensions;
	groups.set(id, group);
	return group;
}

function bindGroup(group, target) {
	group.runtime.target = target;
	if (!group.runtime.bound) group.runtime.bind();
}

/** Calls of a mirror reach the child session as `target`. */
function live(groupId, target) {
	const group = groups.get(groupId);
	if (!group) throw new Error(STALE_MESSAGE);
	bindGroup(group, target);
	return group;
}

async function invoke(groupId, kind, target, payload) {
	const group = live(groupId, target);
	const extension = group.extensions[payload.index];
	const signal = signalFor(payload.signal);
	if (kind === "handler") {
		const handler = [...extension.handlers.values()].flat().find((each) => group.ids.get(each) === payload.id);
		if (!handler) return { value: null, event: null };
		const event = signal ? { ...payload.event, signal } : payload.event;
		const value = await handler(event, createContext(group.runner, target));
		delete event.signal;
		const member = `extensionFactories "${payload.event.type}" handler`;
		return { value: plain(value ?? null, `${member} result`), event: plain(event, `${member} event`) };
	}
	if (kind === "tool") {
		const tool = extension.tools.get(payload.name)?.definition;
		if (!tool) throw new Error(`Tool ${payload.name} is not registered`);
		const update = payload.updateKey;
		const onUpdate = update ? (partial) => call("tool.update", update, normalize(partial)) : undefined;
		const ctx = createToolContext(group.runner, payload.toolCallId, signal, target);
		return normalize(await tool.execute(payload.toolCallId, payload.params, signal, onUpdate, ctx));
	}
	const command = extension.commands.get(payload.name);
	if (kind === "command") {
		await command.handler(payload.args, createCommandContext(group.runner, target));
		return null;
	}
	const items = await command?.getArgumentCompletions?.(payload.prefix);
	return Array.isArray(items) ? items : null;
}

function hook(groupId, target, { index, tool: name, hook: kind, input }) {
	const tool = live(groupId, target).extensions[index]?.tools.get(name)?.definition;
	if (kind === "prepareLoadout") {
		const view = {
			...input,
			getExposure: (each) => input.exposures?.[each] ?? "direct",
			getNamespace: (each) => input.namespaces?.[each],
			getPromptGuidelines: (each) => input.guidelines?.[each] ?? [],
		};
		return tool?.prepareLoadout?.(view) ?? null;
	}
	return tool?.[kind]?.(input) ?? (kind === "reachesTools" ? [] : input);
}

function models(groupId) {
	groups.get(groupId)?.runtime.bindModels();
	return null;
}

/** `host.sdk(name, ...)` calls a child host's mirrors make through Rust. */
export const parentOps = { invoke, hook, bus: busCall, models };

/** End a group: its pi objects go stale and its bus listeners stop (upstream #7193). */
export function releaseGroup(group) {
	if (!group || groups.get(group.id) !== group) return;
	groups.delete(group.id);
	group.runtime.invalidate(STALE_MESSAGE);
	group.runner.invalidate(STALE_MESSAGE);
}

/** Upstream `loadExtensionFactories`. */
async function loadFactories(loader, groupId) {
	const group = createGroup(groupId, loader);
	const errors = [];
	for (const [index, input] of loader.extensionFactories.entries()) {
		const named = typeof input !== "function";
		const path = `<inline:${named ? input.name : index + 1}>`;
		const extension = createExtension(path, path, { path, source: "inline", scope: "temporary", origin: "top-level" });
		const load = createExtensionAPI(extension, group.runtime, loader.cwd, loader.eventBus);
		for (const name of MCP) {
			load.api[name] = () => {
				throw unsupportedError(`extensionFactories pi.${name}`);
			};
		}
		try {
			await (named ? input.factory : input)(load.api);
			load.commit();
		} catch (error) {
			load.discard();
			errors.push({ path, error: error instanceof Error ? error.message : "failed to load extension" });
			continue;
		}
		extension.hidden = named && input.hidden;
		extension.replaceable = named && input.replaceable === true;
		group.extensions.push(extension);
	}
	group.published = true;
	publish(group);
	return { group, errors };
}

function fileExtension(cache, item) {
	let extension = cache.get(item.resolvedPath);
	if (!extension) {
		extension = { path: item.path, resolvedPath: item.resolvedPath };
		for (const member of FILE_ONLY) {
			Object.defineProperty(extension, member, {
				get: () => {
					throw unsupportedError(`DefaultResourceLoader.getExtensions() ${member} of a file extension`);
				},
			});
		}
		cache.set(item.resolvedPath, extension);
	}
	extension.sourceInfo = item.sourceInfo;
	extension.tools = new Map(item.tools.map((tool) => [tool.name, { definition: tool, sourceInfo: tool.sourceInfo }]));
	extension.commands = new Map(item.commands.map((command) => [command.name, command]));
	extension.flags = new Map(item.flags.map((flag) => [flag.name, flag]));
	return extension;
}

const names = (extension) => [
	...[...extension.tools.keys()].map((name) => `tool:${name}`),
	...[...extension.commands.keys()].map((name) => `command:${name}`),
	...[...extension.flags.keys()].map((name) => `flag:${name}`),
];

/** Upstream `omitReplacedExtensions`, `mergeExtensionWarnings`, and `detectExtensionConflicts`. */
function assemble(state, { files, paths, errors, warnings }) {
	const rank = new Map(paths.map((path, index) => [path, index]));
	const ordered = files.map((item) => fileExtension(state.cache, item));
	ordered.sort((a, b) => (rank.get(a.path) ?? Infinity) - (rank.get(b.path) ?? Infinity));
	ordered.push(...(state.group?.extensions ?? []));
	const taken = new Set(ordered.filter((extension) => !extension.replaceable).flatMap(names));
	const extensions = ordered.filter((extension) => !extension.replaceable || !names(extension).some((name) => taken.has(name)));
	const owners = { tools: new Map(), flags: new Map() };
	for (const extension of extensions) {
		for (const kind of ["tools", "flags"]) {
			for (const name of extension[kind].keys()) {
				const owner = owners[kind].get(name);
				if (owner && owner !== extension.path) {
					const label = kind === "tools" ? `Tool "${name}"` : `Flag "--${name}"`;
					errors.push({ path: extension.path, error: `${label} conflicts with ${owner}` });
				} else owners[kind].set(name, extension.path);
			}
		}
	}
	const result = { extensions, errors, warnings: [...new Map(warnings.map((each) => [each.path, each])).values()] };
	if (files.length === 0) result.runtime = state.group?.runtime;
	else {
		Object.defineProperty(result, "runtime", {
			get: () => {
				throw unsupportedError("DefaultResourceLoader.getExtensions().runtime with file extensions");
			},
		});
	}
	return result;
}

/** One phase of upstream `reload()` for extensions: `pretrust`, `final` after it, or `full`. */
export async function loadExtensions(loader, phase) {
	const state = loader._sdkExtensions;
	const factories = loader.extensionFactories.length > 0;
	const loaded = await callAsync("resources.reload", loader._sdkId, phase, factories);
	const reuse = loaded.reused;
	if (!reuse) {
		if (state.group !== state.bound) releaseGroup(state.group);
		if (state.bus !== state.boundBus) closeBus(state.bus);
		state.bus = loaded.group;
		openBus(loaded.group, loader);
		state.group = undefined;
		state.cache = new Map();
	}
	if (!reuse && loaded.group) await callAsync("resources.busReady", loaded.group);
	let factoryErrors = [];
	if (!reuse && factories) ({ group: state.group, errors: factoryErrors } = await loadFactories(loader, loaded.group));
	const finished = await callAsync("resources.finish", loader._sdkId);
	const errors = reuse ? [...state.preErrors, ...loaded.errors] : [...loaded.errors, ...factoryErrors];
	errors.push(...finished.errors);
	const warnings = [...(reuse ? state.preWarnings : []), ...loaded.warnings];
	if (phase === "pretrust") Object.assign(state, { preErrors: [...errors], preWarnings: warnings });
	const result = assemble(state, { files: finished.extensions, paths: loaded.paths, errors, warnings });
	if (phase === "pretrust") return { result, packages: loaded.packages };
	result.errors.push(...loaded.missing);
	let final = result;
	if (loader.extensionsOverride) {
		final = loader.extensionsOverride(result);
		const known = new Set(result.extensions);
		if (final.extensions.some((extension) => !known.has(extension))) {
			throw unsupportedError("DefaultResourceLoader.extensionsOverride adding extensions");
		}
	}
	await callAsync("resources.order", loader._sdkId, final.extensions.map((extension) => extension.path));
	return { result: final, packages: loaded.packages };
}

/** The child session `target` took this reload's extensions; `release()` ends them with the session. */
export function bindExtensions(loader, target) {
	const state = loader._sdkExtensions;
	const [group, bus] = [state.group, state.bus];
	Object.assign(state, { bound: group, boundBus: bus });
	if (group && target !== 0) bindGroup(group, target);
	return {
		release: () => {
			releaseGroup(group);
			closeBus(bus);
		},
	};
}
