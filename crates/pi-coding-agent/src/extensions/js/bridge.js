// Calls into the Rust extension host. `call` ops run synchronously on this thread; `callAsync`
// ops run on tokio and return promises. Values cross as JSON, so functions stay on this side.
import piHost from "pi:host";

export function call(op, ...args) {
	return piHost.call("ext.sync", op, ...args);
}

export function callAsync(op, ...args) {
	return piHost.call("ext.async", op, ...args);
}

/** `undefined` for `null`, since JSON has no `undefined`. */
export function optional(value) {
	return value === null ? undefined : value;
}

// Signals Rust owns: Rust aborts them by id when its cancellation token fires.
const rustSignals = new Map();

export function signalFor(id) {
	if (id === undefined || id === null) return undefined;
	let controller = rustSignals.get(id);
	if (!controller) {
		controller = new AbortController();
		rustSignals.set(id, controller);
	}
	return controller.signal;
}

export function abortSignal(id, reason) {
	const controller = rustSignals.get(id);
	rustSignals.delete(id);
	controller?.abort(reason === undefined ? undefined : new Error(String(reason)));
}

export function releaseSignal(id) {
	rustSignals.delete(id);
}

/** Hand a JavaScript signal to Rust: the id of a token cancelled when the signal aborts. */
export function signalId(signal) {
	if (!signal) return undefined;
	const id = call("signal.create", signal.aborted);
	if (!signal.aborted) {
		signal.addEventListener("abort", () => call("signal.abort", id), { once: true });
	}
	return id;
}

export function errorMessage(error) {
	return error instanceof Error ? error.message : String(error);
}

export function errorStack(error) {
	return error instanceof Error ? error.stack : undefined;
}

// Callbacks Rust invokes by key (tool updates, `done` of custom UI, streamed provider events).
const callbacks = new Map();
let nextCallback = 1;

export function registerCallback(fn) {
	const key = nextCallback++;
	callbacks.set(key, fn);
	return key;
}

export function invokeCallback(key, ...args) {
	return callbacks.get(key)?.(...args);
}

export function releaseCallback(key) {
	callbacks.delete(key);
}
