// The child host of an SDK `DefaultResourceLoader`: mirrors that forward each `extensionFactories` entry
// to the factory in the parent host, and both ends of the `pi.events` bridge to the loader's bus.
import { call, callAsync, optional, signalId } from "./bridge.js";

const PLAIN = new Set([Object.prototype, Array.prototype, null]);

/** `value` when it is JSON data; a function, symbol, class instance, or cycle cannot cross hosts. */
export function plain(value, member, seen = new Set()) {
	const type = typeof value;
	if (type === "function" || type === "symbol" || type === "bigint") {
		throw new TypeError(`${member} must be JSON data in pi-rust`);
	}
	if (value === null || type !== "object") return value;
	if (seen.has(value) || !PLAIN.has(Object.getPrototypeOf(value))) {
		throw new TypeError(`${member} must be JSON data in pi-rust`);
	}
	seen.add(value);
	for (const item of Object.values(value)) plain(item, member, seen);
	seen.delete(value);
	return value;
}

export const wrap = (data, member) => (data === undefined ? {} : { value: plain(data, member) });
export const unwrap = (envelope) => envelope?.value;
export const dataOf = (object) => Object.fromEntries(Object.entries(object).filter(([, value]) => typeof value !== "function"));

const mirrors = new Map();
const forward = (kind, payload) => callAsync("inline.forward", kind, payload);

async function onEvent(index, id, event) {
	const { signal, ...data } = event;
	const reply = await forward("handler", { index, id, event: data, signal: signalId(signal) });
	for (const [key, value] of Object.entries(reply?.event ?? {})) event[key] = value;
	return optional(reply?.value ?? null);
}

function mirrorTool(index, { definition, hooks }) {
	const hook = (name) => (input) => call("inline.hook", { index, tool: definition.name, hook: name, input });
	const tool = {
		...definition,
		execute: (toolCallId, params, signal, onUpdate) =>
			forward("tool", { index, name: definition.name, toolCallId, params, signal: signalId(signal), updateKey: onUpdate?.updateKey }),
	};
	if (hooks.prepareArguments) tool.prepareArguments = hook("prepareArguments");
	if (hooks.reachesTools) tool.reachesTools = hook("reachesTools");
	if (hooks.prepareLoadout) tool.prepareLoadout = (view) => hook("prepareLoadout")(dataOf(view));
	return tool;
}

function apply(index, descriptor) {
	const state = mirrors.get(index);
	if (!state || !descriptor) return;
	const live = new Set(descriptor.handlers.map(([, id]) => id));
	for (const [id, off] of state.handlers) {
		if (live.has(id)) continue;
		off();
		state.handlers.delete(id);
	}
	for (const [type, id] of descriptor.handlers) {
		if (!state.handlers.has(id)) state.handlers.set(id, state.pi.on(type, (event) => onEvent(index, id, event)));
	}
	for (const tool of descriptor.tools) state.pi.registerTool(mirrorTool(index, tool));
	for (const command of descriptor.commands) {
		const options = {
			description: command.description,
			handler: (args) => forward("command", { index, name: command.name, args }),
		};
		if (command.completions) {
			options.getArgumentCompletions = (prefix) => forward("complete", { index, name: command.name, prefix }).then(optional);
		}
		state.pi.registerCommand(command.name, options);
	}
}

/** The factory of mirror `index`: Rust generates one module per mirror that calls this. */
export function mirror(pi, index) {
	mirrors.set(index, { pi, handlers: new Map() });
	apply(index, optional(call("inline.descriptor", index)));
}

/** `pi.events` of this host goes to the loader's bus in the parent host, which delivers it back. */
function bridge(eventBus) {
	const listeners = new Map();
	const send = (kind, channel, envelope) => call("inline.bus", kind, channel, envelope ?? null);
	eventBus.emit = (channel, data) => void send("emit", channel, wrap(data, `pi.events.emit("${channel}") data`));
	eventBus.on = (channel, handler) => {
		const safe = async (data) => {
			try {
				await handler(data);
			} catch (error) {
				console.error(`Event handler error (${channel}):`, error);
			}
		};
		let set = listeners.get(channel);
		if (!set) {
			set = new Set();
			listeners.set(channel, set);
			send("on", channel);
		}
		set.add(safe);
		return () => {
			if (!set.delete(safe) || set.size > 0) return;
			listeners.delete(channel);
			send("off", channel);
		};
	};
	eventBus.deliver = (channel, envelope) => {
		for (const listener of [...(listeners.get(channel) ?? [])]) listener(unwrap(envelope));
	};
}

const buses = new Map();

/** Parent host: the loader's bus for the child host of group `id`, open from its reload until `closeBus`. */
export function openBus(id, loader) {
	if (id) buses.set(id, { loader, forwarders: new Map() });
}

export function closeBus(id) {
	const bus = buses.get(id);
	buses.delete(id);
	for (const off of bus?.forwarders.values() ?? []) off();
}

/** Parent host: a `pi.events` call of the child host of group `id`. */
export function busCall(id, kind, channel, envelope) {
	const bus = buses.get(id);
	if (!bus) return null;
	const eventBus = bus.loader.eventBus;
	if (kind === "emit") eventBus.emit(channel, unwrap(envelope));
	else if (kind === "on" && !bus.forwarders.has(channel)) {
		const member = `eventBus "${channel}" data for an SDK child session`;
		const forward = (data) => call("resources.deliver", id, channel, wrap(data, member));
		bus.forwarders.set(channel, eventBus.on(channel, forward));
	} else if (kind === "off") {
		bus.forwarders.get(channel)?.();
		bus.forwarders.delete(channel);
	}
	return null;
}

const snapshot = (ext) => ({
	path: ext.path,
	resolvedPath: ext.resolvedPath,
	sourceInfo: ext.sourceInfo,
	tools: [...ext.tools.values()].map(({ definition, sourceInfo }) => ({ ...dataOf(definition), sourceInfo })),
	commands: [...ext.commands.values()].map(({ name, description, sourceInfo }) => ({ name, description, sourceInfo })),
	flags: [...ext.flags.values()].map(({ name, description, type, default: value }) => ({ name, description, type, default: value })),
});

/** `host.sdk(name, ...)` calls Rust makes on a child host. */
export const childOps = {
	bridge: (host) => bridge(host.eventBus),
	deliver: (host, channel, envelope) => host.eventBus.deliver?.(channel, envelope),
	mirror: (_host, index, descriptor) => apply(index, descriptor),
	snapshot: (host) => host.runner.extensions.filter((ext) => !ext.path.startsWith("<inline:")).map(snapshot),
	order(host, paths) {
		const rank = new Map(paths.map((path, index) => [path, index]));
		const kept = host.runner.extensions.filter((ext) => rank.has(ext.path));
		kept.sort((a, b) => rank.get(a.path) - rank.get(b.path));
		host.runner.extensions.splice(0, host.runner.extensions.length, ...kept);
		host.runtime.markChanged(true);
		return null;
	},
};
