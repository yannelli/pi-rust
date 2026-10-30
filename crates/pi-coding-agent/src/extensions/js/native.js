// Native extensions (docs/extensions/native-protocol.md): a child process speaks JSON-RPC on
// stdio, and this factory maps the protocol onto the `pi` API it receives.
import { spawn } from "node:child_process";
import { call } from "./bridge.js";
import { hostMethods } from "./native_host.js";
import { autocompleteFactory, createRegistrar } from "./native_register.js";
import { createPeer, readLines, rpcError } from "./native_rpc.js";
import { createSurfaces } from "./native_ui.js";

export const PROTOCOL_VERSION = 1;
const INITIALIZE_MS = 10000;
const SHUTDOWN_MS = 2000;
const KILL_MS = 2000;

const safe = (read, fallback = null) => {
	try {
		return read() ?? fallback;
	} catch {
		return fallback;
	}
};

/** The extension factory of one `pi.native` entry: `{ command, args, env, extensionPath }`. */
export function nativeFactory(spec) {
	return (pi) => loadNative(pi, spec);
}

function createState(pi, spec) {
	const state = {
		pi,
		path: spec.extensionPath,
		ctx: undefined,
		commandCtx: undefined,
		loaded: false,
		dead: false,
		stopping: false,
		autocompleteInstalled: false,
		tools: new Set(),
		events: new Set(),
		flags: [],
		triggers: [],
		updates: new Map(),
		subscriptions: new Map(),
		log: (line) => console.error(`[${spec.extensionPath}] ${line}`),
		currentCtx() {
			const ctx = state.commandCtx ?? state.ctx;
			if (!ctx) throw rpcError(-32002, "No event has reached this extension yet");
			return ctx;
		},
		capture(ctx) {
			if (!ctx) return;
			state.ctx = ctx;
			state.installAutocomplete();
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
		mode: safe(() => call("ctx.mode", 0)),
		hasUI: safe(() => call("ctx.hasUI", 0), false),
		flags: {},
	};
}

function onExit(state, code, signal) {
	if (state.dead) return;
	state.dead = true;
	const reason = signal ? `signal ${signal}` : `code ${code}`;
	const message = `Native extension ${state.path} exited with ${reason}`;
	state.peer.close(new Error(message));
	state.surfaces.closeAll();
	for (const unsubscribe of state.subscriptions.values()) unsubscribe();
	state.subscriptions.clear();
	if (state.stopping || !state.loaded) return;
	state.log(message);
	safe(() => state.ctx?.ui.notify(message, "error"));
	safe(() => state.pi.setActiveTools(state.pi.getActiveTools().filter((name) => !state.tools.has(name))));
}

async function stop(state) {
	if (state.dead || state.stopping) return;
	state.stopping = true;
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
	const handlers = { ...hostMethods(state), ...state.surfaces.methods, ...registrar.methods };
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

