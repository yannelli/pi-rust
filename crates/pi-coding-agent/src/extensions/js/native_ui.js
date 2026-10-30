// Line-based UI of native extensions: widgets, the footer, and `ui.custom` panels render the last
// lines the extension pushed. Width changes and keys go to the extension as notifications.
import { parseKey, truncateToWidth } from "@earendil-works/pi-tui";
import { rpcError } from "./native_rpc.js";

const lineList = (lines) => (Array.isArray(lines) ? lines.map((line) => String(line ?? "")) : []);

export function createSurfaces(state) {
	const surfaces = new Map();

	const component = (id, surface) => {
		const view = {
			render(width) {
				if (surface.width !== width) {
					surface.width = width;
					state.peer.notify("ui.resize", { surface: id, width });
				}
				return surface.lines.map((line) => truncateToWidth(line, width));
			},
			invalidate() {},
		};
		if (surface.kind === "panel") {
			view.handleInput = (data) => state.peer.notify("ui.key", { surface: id, data, key: parseKey(data) ?? null });
		}
		return view;
	};

	const attach = (id, surface, tui) => {
		surface.tui = tui;
		return component(id, surface);
	};

	const push = (surface, lines) => {
		surface.lines = lineList(lines);
		surface.tui?.requestRender();
	};

	const ui = () => state.currentCtx().ui;
	const isTui = () => state.currentCtx().mode === "tui";

	const setWidget = ({ key, lines, placement }) => {
		const id = `widget:${key}`;
		const existing = surfaces.get(id);
		if (lines === null || lines === undefined) {
			surfaces.delete(id);
			ui().setWidget(key, undefined);
			return null;
		}
		if (!isTui()) {
			ui().setWidget(key, lineList(lines), { placement });
			return null;
		}
		if (existing && existing.placement === placement) {
			push(existing, lines);
			return null;
		}
		const surface = { kind: "widget", key, placement, lines: lineList(lines) };
		surfaces.set(id, surface);
		ui().setWidget(key, (tui) => attach(id, surface, tui), { placement });
		return null;
	};

	const setFooter = ({ lines }) => {
		const existing = surfaces.get("footer");
		if (lines === null || lines === undefined) {
			surfaces.delete("footer");
			ui().setFooter(undefined);
			return null;
		}
		if (existing) {
			push(existing, lines);
			return null;
		}
		const surface = { kind: "footer", lines: lineList(lines) };
		surfaces.set("footer", surface);
		ui().setFooter((tui) => attach("footer", surface, tui));
		return null;
	};

	const finish = (surface, result) => {
		if (surface.closed) return;
		surface.closed = true;
		if (surface.done) surface.done(result);
		else surface.pendingResult = { result };
	};

	const custom = async ({ surface: id, lines, overlay, overlayOptions }, signal) => {
		const ctx = state.currentCtx();
		if (!ctx.hasUI) throw rpcError(-32001, "pi has no UI in this mode");
		if (typeof id !== "string" || id === "" || surfaces.has(id)) throw rpcError(-32602, `Surface "${id}" is missing or open`);
		const surface = { kind: "panel", lines: lineList(lines) };
		surfaces.set(id, surface);
		const closeFromHost = () => {
			if (surface.closed) return;
			finish(surface, null);
			state.peer.notify("ui.closed", { surface: id });
		};
		signal?.addEventListener("abort", closeFromHost, { once: true });
		try {
			const result = await ctx.ui.custom(
				(tui, _theme, _keybindings, done) => {
					surface.done = done;
					if (surface.pendingResult) queueMicrotask(() => done(surface.pendingResult.result));
					return attach(id, surface, tui);
				},
				{ overlay: Boolean(overlay), overlayOptions },
			);
			return { result: result ?? null };
		} finally {
			signal?.removeEventListener("abort", closeFromHost);
			surfaces.delete(id);
		}
	};

	const methods = {
		"ui.setWidget": setWidget,
		"ui.setFooter": setFooter,
		"ui.custom": custom,
		"ui.update": ({ surface, lines }) => {
			const target = surfaces.get(surface);
			if (!target) throw rpcError(-32602, `No surface "${surface}"`);
			push(target, lines);
			return null;
		},
		"ui.close": ({ surface, result }) => {
			const target = surfaces.get(surface);
			if (target?.kind === "panel") finish(target, result ?? null);
			else if (target?.kind === "widget") setWidget({ key: target.key, lines: null });
			else if (target?.kind === "footer") setFooter({ lines: null });
			return null;
		},
	};

	/** The extension stopped: close panels and remove its widgets and footer. */
	const closeAll = () => {
		for (const [id, surface] of [...surfaces]) {
			try {
				if (surface.kind === "panel") finish(surface, null);
				else if (surface.kind === "widget") ui().setWidget(surface.key, undefined);
				else ui().setFooter(undefined);
			} catch {
				// The session may be gone already.
			}
			surfaces.delete(id);
		}
	};

	return { methods, closeAll };
}
