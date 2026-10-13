// `ctx.ui` (upstream `ExtensionUIContext`). Dialogs and plain calls go to the Rust `ExtensionUi`
// of the session `target`; component factories stay here and Rust creates their components by id
// when the TUI shows them (`createComponent`).
import { KeybindingsManager, TUI_KEYBINDINGS } from "@earendil-works/pi-tui";
import { call, callAsync, invokeCallback, optional, registerCallback, releaseCallback, signalId } from "./bridge.js";
import { currentTheme, getEditorTheme, themeByName } from "./theme.js";

const factories = new Map();
let nextFactory = 1;

/** Keep a component factory; Rust refers to it by the returned id. */
export function registerFactory(kind, factory) {
	const id = nextFactory++;
	factories.set(id, { kind, factory });
	return id;
}

export function releaseFactory(id) {
	factories.delete(id);
}

/** The `tui` argument of component factories: rendering requests go to the Rust TUI. */
function tuiHandle(target) {
	return {
		requestRender: (force) => call("ui.requestRender", target, Boolean(force)),
		get terminal() {
			const [columns, rows] = call("ui.terminalSize", target);
			return { columns, rows, get width() {
				return columns;
			}, get height() {
				return rows;
			} };
		},
		get columns() {
			return call("ui.terminalSize", target)[0];
		},
		get rows() {
			return call("ui.terminalSize", target)[1];
		},
	};
}

let keybindings;

/** The app keybindings (`keybindings.json` over the defaults) for `matches(data, id)`. */
export function appKeybindings() {
	if (!keybindings) {
		const { definitions, user } = call("ui.keybindings");
		keybindings = new KeybindingsManager({ ...TUI_KEYBINDINGS, ...definitions }, user);
	}
	return keybindings;
}

/** Called by Rust: build the component of factory `id`. `doneKey` resolves a `custom()` call. */
export async function createComponent(id, target, extra) {
	const entry = factories.get(id);
	if (!entry) throw new Error(`Unknown component factory ${id}`);
	const tui = tuiHandle(target);
	const theme = currentTheme();
	switch (entry.kind) {
		case "custom": {
			const done = (result) => call("ui.customDone", id, result === undefined ? null : result);
			return await entry.factory(tui, theme, appKeybindings(), done);
		}
		case "footer":
			return await entry.factory(tui, theme, footerData(target));
		case "editor":
			return await entry.factory(tui, getEditorTheme(), appKeybindings());
		case "autocomplete":
			return entry.factory(extra);
		default:
			return await entry.factory(tui, theme);
	}
}

function footerData(target) {
	return {
		getGitBranch: () => optional(call("ui.footer", target, "gitBranch")),
		getExtensionStatuses: () => new Map(Object.entries(call("ui.footer", target, "statuses") ?? {})),
		getAvailableProviderCount: () => call("ui.footer", target, "providerCount") ?? 0,
		onBranchChange: () => () => {},
	};
}

function dialogOptions(opts) {
	if (!opts) return {};
	return { timeout: opts.timeout, signal: signalId(opts.signal) };
}

const terminalInputHandlers = new Map();

/** Called by Rust for each terminal input chunk; returns the first handler's verdict. */
export function terminalInput(data) {
	let current = data;
	for (const handler of [...terminalInputHandlers.values()]) {
		const result = handler(current);
		if (result?.consume) return { consume: true };
		if (typeof result?.data === "string") current = result.data;
	}
	return current === data ? undefined : { data: current };
}

export function createUiContext(target, prompt) {
	const factoryCall = (op, kind, factory, ...rest) => {
		if (factory === undefined) return call(op, target, null, ...rest);
		return call(op, target, registerFactory(kind, factory), ...rest);
	};
	return {
		select: (title, options, opts) =>
			prompt("select", title, () => callAsync("ui.select", target, title, options, dialogOptions(opts))).then(optional),
		confirm: (title, message, opts) =>
			prompt("confirm", title, () => callAsync("ui.confirm", target, title, message, dialogOptions(opts))),
		input: (title, placeholder, opts) =>
			prompt("input", title, () =>
				callAsync("ui.input", target, title, placeholder ?? null, dialogOptions(opts)).then(optional),
			),
		editor: (title, prefill) =>
			prompt("editor", title, () => callAsync("ui.editor", target, title, prefill ?? null).then(optional)),
		custom: (factory, options) =>
			prompt("custom", undefined, async () => {
				const id = registerFactory("custom", factory);
				try {
					const handleKey = options?.onHandle ? registerCallback(options.onHandle) : undefined;
					const overlay = typeof options?.overlayOptions === "function" ? options.overlayOptions() : options?.overlayOptions;
					const result = await callAsync("ui.custom", target, id, {
						overlay: Boolean(options?.overlay),
						overlayOptions: overlay,
						handleKey,
					});
					if (handleKey !== undefined) releaseCallback(handleKey);
					return optional(result);
				} finally {
					releaseFactory(id);
				}
			}),
		notify: (message, type) => call("ui.notify", target, String(message), type ?? null),
		onTerminalInput: (handler) => {
			const key = registerCallback(handler);
			terminalInputHandlers.set(key, handler);
			call("ui.terminalInput", target, true);
			return () => {
				terminalInputHandlers.delete(key);
				releaseCallback(key);
				if (terminalInputHandlers.size === 0) call("ui.terminalInput", target, false);
			};
		},
		setStatus: (key, text) => call("ui.setStatus", target, key, text ?? null),
		setWorkingMessage: (message) => call("ui.setWorkingMessage", target, message ?? null),
		setWorkingVisible: (visible) => call("ui.setWorkingVisible", target, Boolean(visible)),
		setWorkingIndicator: (options) => call("ui.setWorkingIndicator", target, options ?? null),
		setHiddenThinkingLabel: (label) => call("ui.setHiddenThinkingLabel", target, label ?? null),
		setWidget: (key, content, options) => {
			const placement = options?.placement ?? null;
			if (typeof content === "function") {
				return call("ui.setWidgetComponent", target, key, registerFactory("widget", content), placement);
			}
			return call("ui.setWidget", target, key, content ?? null, placement);
		},
		setFooter: (factory) => factoryCall("ui.setFooter", "footer", factory),
		setHeader: (factory) => factoryCall("ui.setHeader", "header", factory),
		setTitle: (title) => call("ui.setTitle", target, String(title)),
		pasteToEditor: (text) => call("ui.pasteToEditor", target, String(text)),
		setEditorText: (text) => call("ui.setEditorText", target, String(text)),
		getEditorText: () => call("ui.getEditorText", target) ?? "",
		addAutocompleteProvider: (factory) => factoryCall("ui.addAutocompleteProvider", "autocomplete", factory),
		setEditorComponent: (factory) => {
			editorFactory = factory;
			return factoryCall("ui.setEditorComponent", "editor", factory);
		},
		getEditorComponent: () => editorFactory,
		get theme() {
			return currentTheme();
		},
		getAllThemes: () => call("ui.getAllThemes", target).map((info) => ({ name: info.name, path: optional(info.path) })),
		getTheme: (name) => themeByName(name),
		setTheme: (theme) => {
			const name = typeof theme === "string" ? theme : theme?.name;
			if (!name) return { success: false, error: "Theme has no name" };
			const error = optional(call("ui.setTheme", target, name));
			return error === undefined ? { success: true } : { success: false, error };
		},
		getToolsExpanded: () => Boolean(call("ui.getToolsExpanded", target)),
		setToolsExpanded: (expanded) => call("ui.setToolsExpanded", target, Boolean(expanded)),
	};
}

let editorFactory;

/** Called by Rust for an autocomplete provider created by `addAutocompleteProvider`. */
export async function autocomplete(provider, method, args) {
	return await provider[method](...args);
}

export { invokeCallback };
