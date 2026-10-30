// JSON-RPC 2.0 over newline-delimited JSON for native extensions (docs/extensions/native-protocol.md).
import { StringDecoder } from "node:string_decoder";

export const CANCELLED = -32800;

export function rpcError(code, message, data) {
	return Object.assign(new Error(message), { rpcCode: code, rpcData: data });
}

function messageOf(error) {
	return error instanceof Error ? error.message : String(error);
}

/** Call `onLine` for each `\n`-terminated line of `stream`, and for the rest at its end. */
export function readLines(stream, onLine) {
	const decoder = new StringDecoder("utf8");
	let buffer = "";
	const drain = () => {
		let index = buffer.indexOf("\n");
		while (index >= 0) {
			const line = buffer.slice(0, index);
			buffer = buffer.slice(index + 1);
			onLine(line.endsWith("\r") ? line.slice(0, -1) : line);
			index = buffer.indexOf("\n");
		}
	};
	stream.on("data", (chunk) => {
		buffer += typeof chunk === "string" ? chunk : decoder.write(chunk);
		drain();
	});
	stream.on("end", () => {
		buffer += decoder.end();
		drain();
		if (buffer) onLine(buffer);
		buffer = "";
	});
}

/** One side of the connection: `handlers` serve the other side's requests and notifications. */
export function createPeer({ write, handlers, log }) {
	let nextId = 1;
	let closed;
	const pending = new Map();
	const incoming = new Map();
	const send = (message) => {
		if (!closed) write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
	};

	const settle = (id, result, error) => {
		const entry = pending.get(id);
		if (!entry) return;
		pending.delete(id);
		for (const cleanup of entry.cleanup) cleanup();
		if (error) entry.reject(error);
		else entry.resolve(result);
	};

	/** Options: `timeout` (ms), `signal` (sends `$/cancelRequest`), `abortTimeout` and `abortMessage`. */
	const request = (method, params, options = {}) => {
		if (closed) return Promise.reject(closed);
		const id = nextId++;
		return new Promise((resolve, reject) => {
			const entry = { resolve, reject, cleanup: [] };
			pending.set(id, entry);
			const timer = (ms, error) => {
				const handle = setTimeout(() => settle(id, undefined, error), ms);
				entry.cleanup.push(() => clearTimeout(handle));
			};
			if (options.timeout) timer(options.timeout, rpcError(-32603, `${method} timed out after ${options.timeout} ms`));
			const signal = options.signal;
			if (signal) {
				const onAbort = () => {
					send({ method: "$/cancelRequest", params: { id } });
					if (options.abortTimeout !== undefined) {
						timer(options.abortTimeout, rpcError(CANCELLED, options.abortMessage ?? `${method} cancelled`));
					}
				};
				if (signal.aborted) queueMicrotask(onAbort);
				else {
					signal.addEventListener("abort", onAbort, { once: true });
					entry.cleanup.push(() => signal.removeEventListener("abort", onAbort));
				}
			}
			send({ id, method, params: params ?? {} });
		});
	};

	const serve = async ({ id, method, params }) => {
		if (method === "$/cancelRequest") {
			incoming.get(params?.id)?.abort();
			return;
		}
		const handler = handlers[method];
		if (!handler) {
			if (id !== undefined) send({ id, error: { code: -32601, message: `Unknown method ${method}` } });
			return;
		}
		const controller = new AbortController();
		if (id !== undefined) incoming.set(id, controller);
		try {
			const result = await handler(params ?? {}, controller.signal);
			if (id !== undefined) send({ id, result: result === undefined ? null : result });
		} catch (error) {
			const code = error?.rpcCode ?? -32603;
			if (id !== undefined) send({ id, error: { code, message: messageOf(error), data: error?.rpcData } });
			else log(`${method} failed: ${messageOf(error)}`);
		} finally {
			if (id !== undefined) incoming.delete(id);
		}
	};

	const receive = (line) => {
		if (!line.trim()) return;
		let message;
		try {
			message = JSON.parse(line);
		} catch {
			log(`invalid JSON on stdout: ${line.slice(0, 200)}`);
			return;
		}
		if (typeof message?.method === "string") {
			void serve(message);
			return;
		}
		const error = message?.error ? rpcError(message.error.code, message.error.message, message.error.data) : undefined;
		settle(message?.id, message?.result, error);
	};

	const close = (error) => {
		if (closed) return;
		closed = error;
		for (const id of [...pending.keys()]) settle(id, undefined, error);
		for (const controller of incoming.values()) controller.abort();
	};

	return {
		request,
		notify: (method, params) => send({ method, params: params ?? {} }),
		receive,
		close,
		get closed() {
			return closed;
		},
	};
}
