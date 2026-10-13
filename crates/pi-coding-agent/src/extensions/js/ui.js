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
			return attachEditor(await entry.factory(tui, getEditorTheme(), appKeybindings()), target);
		case "autocomplete":
			return entry.factory(extra);
		default:
			return await entry.factory(tui, theme);
	}
}

/** Wire a custom editor to the app like upstream `setCustomEditorComponent`: its submit, change,
 * escape, Ctrl+D, paste, and app actions reach the Rust TUI. */
function attachEditor(editor, target) {
	if (!editor) return editor;
	const send = (kind, value) => call("ui.editorEvent", target, kind, value ?? null);
	editor.onSubmit = (text) => send("submit", text);
	editor.onChange = (text) => send("change", text);
	if (editor.actionHandlers instanceof Map) {
		editor.onEscape ??= () => send("escape");
		editor.onCtrlD ??= () => send("ctrlD");
		editor.onPasteImage ??= () => send("pasteImage");
		editor.onExtensionShortcut ??= (data) => call("ui.editorShortcut", target, data) === true;
		for (const action of call("ui.editorActions", target) ?? []) {
			editor.actionHandlers.set(action, () => send("action", action));
		}
	}
	return editor;
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
					const onHandle = options?.onHandle;
					const handleKey = onHandle ? registerCallback((overlay) => onHandle(overlayHandle(target, overlay))) : undefined;
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

/** The `OverlayHandle` of a `custom()` overlay; calls go to the Rust TUI. */
function overlayHandle(target, id) {
	const op = (method, arg) => call("ui.overlay", target, id, method, arg ?? null);
	return {
		hide: () => op("hide"),
		setHidden: (hidden) => op("setHidden", Boolean(hidden)),
		isHidden: () => Boolean(op("isHidden")),
		focus: () => op("focus"),
		unfocus: () => op("unfocus"),
		isFocused: () => Boolean(op("isFocused")),
		getBounds: () => optional(op("getBounds")),
	};
}

/** The built-in provider as the `current` of the first `addAutocompleteProvider` factory. */
function baseAutocomplete(target) {
	return {
		getSuggestions: async (lines, cursorLine, cursorCol, options) =>
			optional(await callAsync("ui.autocompleteBase", target, lines, cursorLine, cursorCol, Boolean(options?.force))) ??
			null,
		applyCompletion: (lines, cursorLine, cursorCol, item, prefix) =>
			call("ui.autocompleteApply", lines, cursorLine, cursorCol, item, prefix),
		shouldTriggerFileCompletion: (lines, cursorLine, cursorCol) =>
			call("ui.autocompleteFileTrigger", lines, cursorLine, cursorCol),
	};
}

/** Called by Rust: apply the `addAutocompleteProvider` factories `ids` in order to the built-in
 * provider; the result keeps the trigger characters of every provider in the chain. */
export function autocompleteChain(target, ids) {
	let provider = baseAutocomplete(target);
	const triggers = new Set();
	for (const id of ids) {
		const entry = factories.get(id);
		if (!entry) continue;
		provider = entry.factory(provider) ?? provider;
		for (const trigger of provider.triggerCharacters ?? []) triggers.add(trigger);
	}
	return { provider, triggerCharacters: [...triggers] };
}

/** Called by Rust with a chain from `autocompleteChain`. */
export async function autocomplete(chain, method, args) {
	const provider = chain.provider;
	if (typeof provider[method] !== "function") return null;
	return (await provider[method](...args)) ?? null;
}

export { invokeCallback };
