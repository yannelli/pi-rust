// Registrations of native extensions: tools, commands, shortcuts, flags, events, providers,
// virtual models, and autocomplete triggers become `pi.*` registrations that call the child.
import { isObject, mergePatch, snapshot, toJson } from "./native_host.js";
import { createContentRenderers, keyTexts, toolRenderers } from "./native_render.js";

const TOOL_ABORT_MS = 5000;
const PREPARE_MS = 1500;
const TOOL_RESULT_KEYS = ["content", "details", "structuredContent", "isError", "usage"];

/** The handler result of events whose runner reads only the result, built from a patched event. */
const PATCHED_RESULTS = {
	tool_result: (event) => Object.fromEntries(TOOL_RESULT_KEYS.filter((key) => key in event).map((key) => [key, event[key]])),
	message_end: (event) => ({ message: event.message }),
	context: (event) => ({ messages: event.messages }),
	context_with_system: (event) => ({ messages: event.messages }),
};

function textResult(text) {
	return { content: [{ type: "text", text }], isError: true };
}

/** The register functions over `state`, for `initialize` and the `register.*` requests. */
export function createRegistrar(state) {
	const { pi } = state;
	const request = (method, params, options) => state.peer.request(method, params, options);
	const withCtx = (ctx) => {
		state.capture(ctx);
		return snapshot(state, ctx);
	};

	const forward = async (name, event, ctx) => {
		if (state.dead) return undefined;
		const reply = await request("event", { name, event: toJson(event), ctx: withCtx(ctx) }, { signal: event?.signal });
		const result = reply?.result ?? undefined;
		const patch = reply?.patch;
		if (patch === undefined || patch === null) return result;
		if (!isObject(patch)) {
			state.log(`${name}: ignored a patch that is not a JSON object`);
			return result;
		}
		mergePatch(event, patch);
		return result ?? PATCHED_RESULTS[name]?.(event);
	};

	/** Runs before validation; the Rust hook blocks on the returned promise for at most 2 seconds. */
	const prepare = (name) => (args) => {
		if (state.dead) return args;
		return request("tool.prepare", { name, args: toJson(args) }, { timeout: PREPARE_MS });
	};

	const tool = (definition) => {
		state.tools.add(definition.name);
		pi.registerTool({
			...toolRenderers(state, definition),
			prepareArguments: definition.prepareArguments ? prepare(definition.name) : undefined,
			name: definition.name,
			label: definition.label ?? definition.name,
			description: definition.description ?? "",
			parameters: definition.parameters ?? { type: "object", properties: {} },
			promptSnippet: definition.promptSnippet,
			promptGuidelines: definition.promptGuidelines,
			executionMode: definition.executionMode,
			async execute(toolCallId, params, signal, onUpdate, ctx) {
				if (state.dead) return textResult(`Native extension ${state.path} is not running`);
				if (onUpdate) state.updates.set(toolCallId, onUpdate);
				try {
					const payload = { toolCallId, name: definition.name, params: params ?? {}, ctx: withCtx(ctx) };
					const options = { signal, abortTimeout: TOOL_ABORT_MS, abortMessage: "Tool call aborted" };
					const result = await request("tool.execute", payload, options);
					return { ...result, content: Array.isArray(result?.content) ? result.content : [] };
				} finally {
					state.updates.delete(toolCallId);
				}
			},
		});
	};

	const command = ({ name, description, completions }) => {
		pi.registerCommand(name, {
			description,
			getArgumentCompletions: completions ? (prefix) => request("command.complete", { name, prefix }) : undefined,
			handler: async (args, ctx) => {
				const outer = state.commandCtx;
				state.commandCtx = ctx;
				try {
					await request("command.run", { name, args: args ?? "", ctx: withCtx(ctx) });
				} finally {
					state.commandCtx = outer;
				}
			},
		});
	};

	const shortcut = ({ shortcut: key, description }) => {
		pi.registerShortcut(key, {
			description,
			handler: (ctx) => request("shortcut.run", { shortcut: key, ctx: withCtx(ctx) }),
		});
	};

	const flag = ({ name, description, type, default: value }) => {
		if (!state.flags.includes(name)) state.flags.push(name);
		pi.registerFlag(name, { description, type: type ?? "boolean", default: value });
	};

	const events = (names) => {
		for (const name of names ?? []) {
			if (state.events.has(name)) continue;
			state.events.add(name);
			if (name === "footer_changed") state.watchFooter();
			else if (name !== "session_start" && name !== "session_shutdown") pi.on(name, (event, ctx) => forward(name, event, ctx));
		}
	};

	const virtualModel = (definition) => {
		pi.registerVirtualModel({
			...definition,
			route: (routeRequest, ctx) => {
				const params = { provider: definition.provider, id: definition.id, request: toJson(routeRequest), ctx: withCtx(ctx) };
				return request("virtualModel.route", params, { signal: routeRequest?.signal });
			},
		});
	};

	const autocomplete = ({ triggerCharacters }) => {
		for (const trigger of triggerCharacters ?? []) if (!state.triggers.includes(trigger)) state.triggers.push(trigger);
		state.installAutocomplete();
	};

	const content = createContentRenderers(state);

	const provider = (name, config) => {
		pi.registerProvider(name, config);
		if (isObject(config?.oauth)) state.oauthProviders.add(name);
		else state.oauthProviders.delete(name);
	};

	const apply = (init) => {
		for (const definition of init.tools ?? []) tool(definition);
		for (const definition of init.commands ?? []) command(definition);
		for (const definition of init.shortcuts ?? []) shortcut(definition);
		for (const definition of init.flags ?? []) flag(definition);
		for (const { name, config } of init.providers ?? []) provider(name, config);
		for (const definition of init.virtualModels ?? []) virtualModel(definition);
		for (const definition of init.autocomplete ?? []) autocomplete(definition);
		for (const customType of init.messageRenderers ?? []) content.message(customType);
		for (const customType of init.entryRenderers ?? []) content.entry(customType);
		events(init.events);
	};

	const methods = {
		"register.tool": (p) => (tool(p), null),
		"register.command": (p) => (command(p), null),
		"register.shortcut": (p) => (shortcut(p), null),
		"register.flag": (p) => (flag(p), null),
		"register.events": (p) => (events(p.events), null),
		"register.autocomplete": (p) => (autocomplete(p), null),
		"register.messageRenderer": (p) => (content.message(p.customType), null),
		"register.entryRenderer": (p) => (content.entry(p.customType), null),
		"keybindings.get": (p) => keyTexts(p),
		"provider.register": (p) => (provider(p.name, p.config), null),
		"provider.unregister": (p) => {
			state.oauthProviders.delete(p.name);
			pi.unregisterProvider(p.name);
			return null;
		},
		"virtualModel.register": (p) => (virtualModel(p), null),
		"virtualModel.unregister": (p) => (pi.unregisterVirtualModel(p.provider, p.id), null),
		"tool.update": ({ toolCallId, partial }) => {
			state.updates.get(toolCallId)?.({ content: [], ...partial });
			return null;
		},
	};

	return { apply, forward, methods };
}

/** `reply` with pi's own items after its items when both replace the same prefix. */
function merged({ items, prefix }, own) {
	const mine = Array.isArray(items) ? items : [];
	if (!own || own.prefix !== prefix) return { items: mine, prefix };
	const values = new Set(mine.map((item) => item.value));
	return { items: [...mine, ...(own.items ?? []).filter((item) => !values.has(item.value))], prefix };
}

/** The `addAutocompleteProvider` factory: tokens that start with a trigger go to the child. */
export function autocompleteFactory(state) {
	return (current) => ({
		triggerCharacters: state.triggers,
		async getSuggestions(lines, cursorLine, cursorCol, options) {
			const before = (lines[cursorLine] ?? "").slice(0, cursorCol);
			const token = before.slice(before.search(/\S*$/));
			if (!state.dead && state.triggers.some((trigger) => token.startsWith(trigger))) {
				const params = { lines, cursorLine, cursorCol, force: Boolean(options?.force) };
				const reply = await state.peer.request("autocomplete.suggest", params).catch(() => null);
				if (reply?.merge) return merged(reply, await current.getSuggestions(lines, cursorLine, cursorCol, options));
				if (reply) return reply;
			}
			return current.getSuggestions(lines, cursorLine, cursorCol, options);
		},
		applyCompletion: (...args) => current.applyCompletion(...args),
		shouldTriggerFileCompletion: (...args) => current.shouldTriggerFileCompletion?.(...args) ?? false,
	});
}
