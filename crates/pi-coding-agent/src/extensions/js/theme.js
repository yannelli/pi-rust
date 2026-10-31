// Themes for extension components (upstream `modes/interactive/theme/theme.ts`): the ANSI tables
// come from the Rust theme registry, so colors match the TUI. The current theme refreshes once
// per task, which picks up theme switches without a call per `fg`.
import { call, optional } from "./bridge.js";

function wrap(text, open, close) {
	if (!text) return "";
	const body = text.includes("\x1b") ? text.split(close).join(close + open) : text;
	return body
		.split("\n")
		.map((line) => open + line + close)
		.join("\n");
}

export class Theme {
	#snapshot;

	constructor(snapshot) {
		this.#snapshot = snapshot;
		this.name = optional(snapshot.name);
		this.sourcePath = optional(snapshot.sourcePath);
	}

	get appearance() {
		return this.#snapshot.appearance;
	}

	get colors() {
		return this.#snapshot.colors;
	}

	#ansi(table, token) {
		const value = table[token];
		if (value === undefined) throw new Error(`Unknown theme color: ${token}`);
		return value;
	}

	style(text, options = {}) {
		let result = text;
		if (options.fg) result = this.fg(options.fg, result);
		if (options.bg) result = this.bg(options.bg, result);
		if (options.bold) result = this.bold(result);
		if (options.italic) result = this.italic(result);
		if (options.underline) result = this.underline(result);
		if (options.inverse) result = this.inverse(result);
		if (options.strikethrough) result = this.strikethrough(result);
		return result;
	}

	fg(color, text) {
		const ansi = this.#ansi(this.#snapshot.fg, color);
		if (this.#snapshot.dim.includes(color)) return `${ansi}\x1b[2m${text}\x1b[22;39m`;
		return `${ansi}${text}\x1b[39m`;
	}

	bg(color, text) {
		return `${this.#ansi(this.#snapshot.bg, color)}${text}\x1b[49m`;
	}

	bold(text) {
		return wrap(text, "\x1b[1m", "\x1b[22m");
	}

	italic(text) {
		return wrap(text, "\x1b[3m", "\x1b[23m");
	}

	underline(text) {
		return wrap(text, "\x1b[4m", "\x1b[24m");
	}

	inverse(text) {
		return wrap(text, "\x1b[7m", "\x1b[27m");
	}

	strikethrough(text) {
		return wrap(text, "\x1b[9m", "\x1b[29m");
	}

	getFgAnsi(color) {
		const ansi = this.#ansi(this.#snapshot.fg, color);
		return this.#snapshot.dim.includes(color) ? `${ansi}\x1b[2m` : ansi;
	}

	getBgAnsi(color) {
		return this.#ansi(this.#snapshot.bg, color);
	}

	getColorMode() {
		return this.#snapshot.mode;
	}

	getThinkingBorderColor(level) {
		const tokens = {
			off: "thinkingOff",
			minimal: "thinkingMinimal",
			low: "thinkingLow",
			medium: "thinkingMedium",
			high: "thinkingHigh",
			xhigh: "thinkingXhigh",
			max: "thinkingMax",
		};
		const token = tokens[level] ?? "thinkingOff";
		return (text) => this.fg(token, text);
	}

	getBashModeBorderColor() {
		return (text) => this.fg("bashMode", text);
	}
}

let current;
let fresh = false;

/** The theme the UI shows now. */
export function currentTheme() {
	if (!fresh || !current) {
		current = new Theme(call("theme.current"));
		fresh = true;
		queueMicrotask(() => {
			fresh = false;
		});
	}
	return current;
}

/** A theme by name, or `undefined` when it does not exist. */
export function themeByName(name) {
	const snapshot = optional(call("theme.byName", name));
	return snapshot ? new Theme(snapshot) : undefined;
}

/** The current theme, resolved on every access (upstream's global `theme` proxy). */
export const theme = new Proxy(
	{},
	{
		get(_target, key) {
			const target = currentTheme();
			const value = target[key];
			return typeof value === "function" ? value.bind(target) : value;
		},
	},
);

export function highlightCode(code, lang) {
	return call("theme.highlight", code, lang ?? null);
}

/** Upstream `getAvailableThemes`: the system theme, built-in themes, then custom themes. */
export function getAvailableThemes() {
	return call("theme.available");
}

export function getMarkdownTheme() {
	return {
		heading: (text) => theme.fg("mdHeading", text),
		link: (text) => theme.fg("mdLink", text),
		linkUrl: (text) => theme.fg("mdLinkUrl", text),
		code: (text) => theme.fg("mdCode", text),
		codeBlock: (text) => theme.fg("mdCodeBlock", text),
		codeBlockBorder: (text) => theme.fg("mdCodeBlockBorder", text),
		quote: (text) => theme.fg("mdQuote", text),
		quoteBorder: (text) => theme.fg("mdQuoteBorder", text),
		hr: (text) => theme.fg("mdHr", text),
		listBullet: (text) => theme.fg("mdListBullet", text),
		bold: (text) => theme.bold(text),
		italic: (text) => theme.italic(text),
		underline: (text) => theme.underline(text),
		strikethrough: (text) => theme.strikethrough(text),
		highlightCode: (code, lang) => highlightCode(code, lang),
	};
}

export function getSelectListTheme() {
	return {
		selectedPrefix: (text) => theme.fg("accent", text),
		selectedText: (text) => theme.fg("accent", text),
		description: (text) => theme.fg("muted", text),
		scrollInfo: (text) => theme.fg("muted", text),
		noMatch: (text) => theme.fg("muted", text),
	};
}

export function getEditorTheme() {
	return { borderColor: (text) => theme.fg("borderMuted", text), selectList: getSelectListTheme() };
}

export function getSettingsListTheme() {
	return {
		label: (text, selected) => (selected ? theme.fg("accent", text) : text),
		value: (text, selected) => (selected ? theme.fg("accent", text) : theme.fg("muted", text)),
		description: (text) => theme.fg("dim", text),
		cursor: theme.fg("accent", "→ "),
		hint: (text) => theme.fg("dim", text),
	};
}
