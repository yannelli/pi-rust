// Native extensions (docs/extensions/native-protocol.md): a child process speaks JSON-RPC on
// stdio, and this factory maps the protocol onto the `pi` API it receives.
import { spawn } from "node:child_process";
import { call } from "./bridge.js";
import { footerJson, hostMethods, needsCtx } from "./native_host.js";
import { autocompleteFactory, createRegistrar } from "./native_register.js";
import { createPeer, readLines, rpcError } from "./native_rpc.js";
import { createSurfaces } from "./native_ui.js";
import { NOTIFY_SOURCE } from "./ui.js";

export const PROTOCOL_VERSION = 1;
const INITIALIZE_MS = 10000;
const SHUTDOWN_MS = 2000;
const KILL_MS = 2000;
const CTX_WAIT_MS = 30000;
const POLL_MS = 250;
/** Optional protocol parts this host speaks, sent in `initialize` as `features`. */
const FEATURES = ["toolRenderers", "render.spinner", "render.refresh", "ui.getTheme", "theme_changed", "ui.working", "ui.chatStyle", "contextEntries", "toolResultExpander", "providerAuth"];
/** What `footer_changed` and `theme_changed` subscribers watch: the reader and the notification. */
const WATCHED = {
	footer: { read: () => footerJson(), notify: (value) => ["footer.changed", { footer: value }] },
	theme: { read: () => call("theme.current"), notify: (value) => ["theme.changed", { theme: value }] },
};

const safe = (read, fallback = null) => {
	try {
		return read() ?? fallback;
	} catch {
		return fallback;
	}
};

const safeText = (read) => JSON.stringify(safe(read));

const LOG_LEVEL = /^(error|warn|info|debug):\s*/;
const NOTIFY_TYPE = { error: "error", warn: "warning", info: "info", debug: "info" };

/** Whether pi draws the TUI now: stderr output would land in the editor box. */
const inTui = (state) => safe(() => state.ctx?.mode) === "tui" && safe(() => state.ctx.hasUI, false);

/** A stderr line of the child: a notification in the TUI (like upstream `ctx.ui.notify`), else stderr. */
function logLine(state, line) {
	const level = LOG_LEVEL.exec(line);
	const text = `[${state.path}] ${level ? line.slice(level[0].length) : line}`;
	if (inTui(state)) {
		const type = level ? NOTIFY_TYPE[level[1]] : "warning";
		if (safe(() => (state.ctx.ui.notify(text, type, { [NOTIFY_SOURCE]: state.path }), true), false)) return;
	}
	console.error(`[${state.path}] ${line}`);
}

/** The extension factory of one `pi.native` entry: `{ command, args, env, extensionPath }`. */
export function nativeFactory(spec) {
	return (pi) => loadNative(pi, spec);
}

function createState(pi, spec) {
	const state = {
		pi,
		spec,
		path: spec.extensionPath,
		ctx: undefined,
		commandCtx: undefined,
		loaded: false,
		dead: false,
		stopping: false,
		autocompleteInstalled: false,
		tools: new Set(),
		oauthProviders: new Set(),
		events: new Set(),
		flags: [],
		triggers: [],
		updates: new Map(),
		subscriptions: new Map(),
		log: (line) => logLine(state, line),
		currentCtx() {
			const ctx = state.commandCtx ?? state.ctx;
			if (!ctx) throw rpcError(-32002, "No event has reached this extension yet");
			return ctx;
		},
		capture(ctx) {
			if (!ctx) return;
			state.ctx = ctx;
			for (const resolve of state.ctxWaiters.splice(0)) resolve();
			state.installAutocomplete();
			state.startPoll();
		},
		ctxWaiters: [],
		/** Requests sent before the first event wait for its ctx. */
		whenCtx() {
			if (state.ctx || state.commandCtx) return Promise.resolve();
			return new Promise((resolve) => {
				state.ctxWaiters.push(resolve);
				setTimeout(resolve, CTX_WAIT_MS).unref?.();
			});
		},
		/** The last JSON text of each watched kind (`footer`, `theme`); undefined until the first check. */
		watched: new Map(),
		pollTimer: undefined,
		watch(kind) {
			if (!state.watched.has(kind)) state.watched.set(kind, state.pollTimer ? safeText(WATCHED[kind].read) : undefined);
			state.startPoll();
		},
		/** `footer.changed` and `theme.changed` for their subscribers: one check per POLL_MS in `tui` mode. */
		startPoll() {
			const idle = state.watched.size === 0 || state.pollTimer || state.dead || state.stopping;
			if (idle || safe(() => state.ctx?.mode) !== "tui") return;
			for (const [kind, last] of state.watched) if (last === undefined) state.watched.set(kind, safeText(WATCHED[kind].read));
			state.pollTimer = setInterval(() => {
				for (const [kind, last] of state.watched) {
					const value = safe(WATCHED[kind].read);
					const text = JSON.stringify(value);
					if (text === last || value === null) continue;
					state.watched.set(kind, text);
					state.peer.notify(...WATCHED[kind].notify(value));
				}
			}, POLL_MS);
			state.pollTimer.unref?.();
		},
		stopPoll() {
			clearInterval(state.pollTimer);
			state.pollTimer = undefined;
		},
		installAutocomplete() {
			if (state.autocompleteInstalled || state.triggers.length === 0 || !safe(() => state.ctx?.hasUI, false)) return;
			state.autocompleteInstalled = true;
			state.ctx.ui.addAutocompleteProvider(autocompleteFactory(state));
		},
	};
	return state;
}

function initializeParams(state) {
	const info = safe(() => call("config.info"), {});
	return {
		protocolVersion: PROTOCOL_VERSION,
		piVersion: info.version ?? null,
		cwd: process.cwd(),
		agentDir: info.agentDir ?? null,
		extensionPath: state.path,
		platform: process.platform,
		mode: info.loadMode?.mode ?? safe(() => call("ctx.mode", 0)),
		hasUI: info.loadMode?.hasUI ?? safe(() => call("ctx.hasUI", 0), false),
		flags: {},
		features: FEATURES,
	};
}

function onExit(state, code, signal) {
	if (state.dead) return;
	state.dead = true;
	state.stopPoll();
	const reason = signal ? `signal ${signal}` : `code ${code}`;
	const message = `Native extension ${state.path} exited with ${reason}`;
	state.peer.close(new Error(message));
	state.surfaces.closeAll();
	for (const unsubscribe of state.subscriptions.values()) unsubscribe();
	state.subscriptions.clear();
	if (state.stopping || !state.loaded) return;
	if (!inTui(state)) state.log(message);
	safe(() => state.ctx?.ui.notify(message, "error"));
	safe(() => state.spec.forgetTools?.([...state.tools]));
}

async function stop(state) {
	if (state.dead || state.stopping) return;
	state.stopping = true;
	state.stopPoll();
	await state.peer.request("shutdown", {}, { timeout: SHUTDOWN_MS }).catch(() => undefined);
	state.peer.notify("exit");
	state.child.stdin.end();
	const timer = setTimeout(() => {
		if (!state.dead) state.child.kill("SIGKILL");
	}, KILL_MS);
	timer.unref?.();
}

async function loadNative(pi, spec) {
	const state = createState(pi, spec);
	const child = spawn(spec.command, spec.args ?? [], {
		env: { ...process.env, ...(spec.env ?? {}) },
		stdio: ["pipe", "pipe", "pipe"],
	});
	state.child = child;
	state.surfaces = createSurfaces(state);
	const registrar = createRegistrar(state);
	const contextual = Object.entries({ ...hostMethods(state), ...state.surfaces.methods }).map(([method, handler]) => [
		method,
		async (params, signal) => {
			if (needsCtx(method, params)) await state.whenCtx();
			return handler(params, signal);
		},
	]);
	const handlers = { ...Object.fromEntries(contextual), ...registrar.methods };
	state.peer = createPeer({ write: (text) => child.stdin.write(text), handlers, log: state.log });
	readLines(child.stdout, state.peer.receive);
	readLines(child.stderr, state.log);
	child.stdin.on("error", (error) => state.log(`stdin: ${error.message}`));
	child.on("error", (error) => {
		state.peer.close(new Error(`Failed to start native extension ${spec.command}: ${error.message}`));
	});
	child.on("exit", (code, signal) => onExit(state, code, signal));

	let init;
	try {
		init = await state.peer.request("initialize", initializeParams(state), { timeout: INITIALIZE_MS });
	} catch (error) {
		child.kill();
		throw error;
	}
	if (init?.protocolVersion !== PROTOCOL_VERSION) {
		child.kill();
		throw new Error(`native extension speaks protocol ${init?.protocolVersion}; pi speaks 1..${PROTOCOL_VERSION}`);
	}
	registrar.apply(init);
	pi.on("session_start", (event, ctx) => {
		state.capture(ctx);
		return state.events.has("session_start") ? registrar.forward("session_start", event, ctx) : undefined;
	});
	pi.on("session_shutdown", async (event, ctx) => {
		let result;
		try {
			if (state.events.has("session_shutdown")) result = await registrar.forward("session_shutdown", event, ctx);
		} finally {
			await stop(state);
		}
		return result;
	});
	state.loaded = true;
	state.peer.notify("initialized");
}

