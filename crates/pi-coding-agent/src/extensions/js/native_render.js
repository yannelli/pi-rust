// Custom renderers of native extensions: the child draws the lines of tool calls, tool results,
// custom messages, and session entries. Components answer from a cache and never wait for the child.
import { truncateToWidth } from "@earendil-works/pi-tui";
import { call } from "./bridge.js";
import { toJson } from "./native_host.js";
import { appKeybindings } from "./ui.js";

const RENDER_MS = 5000;
const WIDTHS_KEPT = 4;
const COMPONENTS_KEPT = 256;
const NATIVE = Symbol("nativeRender");
/** A zero-width APC token in rendered lines: pi draws the current spinner frame in its place. */
export const SPINNER_TOKEN = "\x1b_pi:spinner\x07";
const SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_MS = 80;
const MIN_REFRESH_MS = 250;

const lineList = (lines) => (Array.isArray(lines) ? lines.map((line) => String(line ?? "")) : []);
const later = (run, ms) => setTimeout(run, ms).unref?.();
const requestRender = () => {
	try {
		call("ui.requestRender", 0, false);
	} catch {
		// No UI is bound.
	}
};

/** A component that draws the lines `method` returns for `params` and the render width. */
function remoteComponent(state, method) {
	let params;
	let key;
	let redraw = requestRender;
	let generation = 0;
	let inflight = false;
	let ticking = false;
	let shown = [];
	const widths = new Map();

	/** `refreshMs` of an answer: ask again after that time while the inputs stay the same. */
	const refreshAfter = (ms, asked) => {
		if (!Number.isFinite(ms) || ms <= 0) return;
		later(() => {
			if (asked !== generation || state.dead) return;
			generation++;
			widths.clear();
			redraw();
		}, Math.max(MIN_REFRESH_MS, ms));
	};

	/** The current spinner frame in place of each token, and one more frame later. */
	const animate = (lines) => {
		if (!lines.some((line) => line.includes(SPINNER_TOKEN))) return lines;
		if (!ticking) {
			ticking = true;
			later(() => {
				ticking = false;
				requestRender();
			}, SPINNER_MS);
		}
		const frame = SPINNER_FRAMES[Math.floor(Date.now() / SPINNER_MS) % SPINNER_FRAMES.length];
		return lines.map((line) => line.split(SPINNER_TOKEN).join(frame));
	};

	const fetch = (width) => {
		if (inflight || state.dead) return;
		inflight = true;
		const asked = generation;
		const store = (lines) => {
			widths.delete(width);
			widths.set(width, lines);
			if (widths.size > WIDTHS_KEPT) widths.delete(widths.keys().next().value);
		};
		state.peer
			.request(method, { ...params, width }, { timeout: RENDER_MS })
			.then(
				(reply) => {
					if (asked !== generation) return;
					store(lineList(reply?.lines));
					refreshAfter(Number(reply?.refreshMs), asked);
				},
				(error) => {
					if (asked !== generation) return;
					store(shown);
					state.log(`${method} failed: ${error?.message ?? error}`);
				},
			)
			.finally(() => {
				inflight = false;
				redraw();
			})
			.catch(() => undefined);
	};

	return {
		[NATIVE]: method,
		/** New inputs: later frames fetch again; the old lines stay until the new ones arrive. */
		update(next, onRedraw) {
			if (onRedraw) redraw = onRedraw;
			const nextKey = JSON.stringify(next);
			if (nextKey === key) return;
			params = next;
			key = nextKey;
			generation++;
			widths.clear();
		},
		render(width) {
			const lines = widths.get(width);
			if (lines) shown = lines;
			else fetch(width);
			return animate(shown).map((line) => truncateToWidth(line, width));
		},
		/** The host invalidates on events such as a theme change: ask the child again, keep the lines. */
		invalidate() {
			generation++;
			widths.clear();
		},
	};
}

/** Components of message and entry renderers, by their inputs: a rebuild keeps the lines on screen
 * and asks the child again. */
function componentCache(state, method) {
	const kept = new Map();
	return (params) => {
		const key = JSON.stringify(params);
		let component = kept.get(key);
		if (component) {
			kept.delete(key);
			component.invalidate();
		} else {
			component = remoteComponent(state, method);
			component.update(params);
		}
		kept.set(key, component);
		if (kept.size > COMPONENTS_KEPT) kept.delete(kept.keys().next().value);
		return component;
	};
}

/** The render context of a tool row without functions and the per-frame `durationMs`. */
function toolContext(context) {
	const { args, toolCallId, cwd, executionStarted, argsComplete, isPartial, expanded, showImages, isError } = context ?? {};
	return toJson({ args, toolCallId, cwd, executionStarted, argsComplete, isPartial, expanded, showImages, isError });
}

/** `renderCall` and `renderResult` functions for the tool definition `definition`. */
export function toolRenderers(state, definition) {
	const draw = (method, params, context) => {
		if (state.dead) return undefined;
		const last = context?.lastComponent;
		const component = last?.[NATIVE] === method ? last : remoteComponent(state, method);
		component.update(params, () => (context?.invalidate ? context.invalidate() : requestRender()));
		return component;
	};
	const renderers = {};
	if (definition.renderCall) {
		renderers.renderCall = (args, _theme, context) => {
			const params = { toolName: definition.name, toolCallId: context?.toolCallId, args: toJson(args), context: toolContext(context) };
			return draw("render.toolCall", params, context);
		};
	}
	if (definition.renderResult) {
		renderers.renderResult = (result, options, _theme, context) => {
			const params = {
				toolName: definition.name,
				toolCallId: context?.toolCallId,
				result: toJson(result),
				options: { expanded: Boolean(options?.expanded), isPartial: Boolean(options?.isPartial) },
				context: toolContext(context),
			};
			return draw("render.toolResult", params, context);
		};
	}
	if (definition.renderShell === "self" || definition.renderShell === "default") renderers.renderShell = definition.renderShell;
	return renderers;
}

/** `toolRenderers` of `initialize` and `register.toolRenderer`: rows of tools that no other
 * renderer draws, such as the built-in tools. One resolver serves every tool name. */
export function createToolOverrides(state) {
	const overrides = new Map();
	let installed = false;
	/** Adds one definition; false when its `toolName` is not a non-empty string. */
	return (definition) => {
		const name = definition?.toolName;
		if (typeof name !== "string" || name === "") return false;
		overrides.set(name, toolRenderers(state, { ...definition, name }));
		if (installed) return true;
		installed = true;
		state.pi.registerToolRenderer((toolName, next) => {
			const base = next();
			const own = overrides.get(toolName);
			if (!own || state.dead || base?.renderCall || base?.renderResult) return base;
			return own;
		});
		return true;
	};
}

/** `pi.registerMessageRenderer` and `pi.registerEntryRenderer` for the given custom types. */
export function createContentRenderers(state) {
	const messages = componentCache(state, "render.message");
	const entries = componentCache(state, "render.entry");
	const options = (value) => ({ expanded: Boolean(value?.expanded) });
	return {
		message(customType) {
			state.pi.registerMessageRenderer(customType, (message, renderOptions) => {
				if (state.dead) return undefined;
				return messages({ customType, message: toJson(message), options: options(renderOptions) });
			});
		},
		entry(customType) {
			state.pi.registerEntryRenderer(customType, (entry, renderOptions) => {
				if (state.dead) return undefined;
				return entries({ customType, entry: toJson(entry), options: options(renderOptions) });
			});
		},
	};
}

const keyPart = (part) => (process.platform === "darwin" && part.toLowerCase() === "alt" ? "option" : part);

/** `keybindings.get`: the host's key text of each app keybinding id, as upstream `keyText`. */
export function keyTexts({ ids } = {}) {
	const manager = appKeybindings();
	const wanted = Array.isArray(ids) ? ids.map(String) : Object.keys(manager.getResolvedBindings());
	const text = (id) => manager.getKeys(id).map((key) => key.split("+").map(keyPart).join("+")).join("/");
	return Object.fromEntries(wanted.map((id) => [id, text(id)]));
}
