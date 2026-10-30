// In the TUI, stderr output of extensions would land at the cursor, inside the prompt box. There,
// `console.error`, `console.warn`, and `process.stderr.write` become warnings in the chat.
import { formatWithOptions } from "node:util";
import { call } from "./bridge.js";

const inTui = () => {
	try {
		return call("ctx.mode", 0) === "tui";
	} catch {
		return false;
	}
};

/** The short name of `path`: the file name, with its folder for `index.*` files. */
export function extensionName(path) {
	const parts = String(path).split(/[\\/]/).filter(Boolean);
	const last = parts.length - (/^index\.[cm]?[jt]s$/.test(parts.at(-1) ?? "") ? 2 : 1);
	return parts.slice(Math.max(last, 0)).join("/") || String(path);
}

/** The extension whose code is on the stack, else `undefined`. */
function caller(extensions) {
	const stack = (new Error().stack ?? "").replace(/\\/g, "/");
	for (const ext of extensions()) {
		if (ext.resolvedPath && stack.includes(ext.resolvedPath.replace(/\\/g, "/"))) return ext;
	}
	return undefined;
}

/** Route stderr of `extensions()` (`{ path, resolvedPath }`) to `ctx.ui.notify` while in the TUI. */
export function routeStderr(extensions) {
	let busy = false;
	const notify = (text) => {
		const lines = text.replace(/\n+$/, "");
		if (busy || lines === "" || !inTui()) return false;
		busy = true;
		try {
			const ext = caller(extensions);
			const label = ext ? `[${extensionName(ext.path)}] ` : "";
			call("ui.notify", 0, `${label}${lines}`, "warning");
			return true;
		} catch {
			return false;
		} finally {
			busy = false;
		}
	};
	for (const method of ["error", "warn"]) {
		const original = console[method];
		console[method] = (...args) => {
			if (!notify(formatWithOptions({ colors: false }, ...args))) original(...args);
		};
	}
	const stderr = globalThis.process?.stderr;
	if (stderr && typeof stderr.write === "function") {
		const write = stderr.write.bind(stderr);
		stderr.write = (chunk, ...rest) => {
			const text = typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk);
			if (!notify(text)) return write(chunk, ...rest);
			rest.find((arg) => typeof arg === "function")?.();
			return true;
		};
	}
}
