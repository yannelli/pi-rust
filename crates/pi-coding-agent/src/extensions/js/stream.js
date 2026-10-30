// Assistant message streams produced in Rust: Rust pushes each event through a callback key and
// resolves the op with the final message.
import { createAssistantMessageEventStream } from "../pi-ai/src/utils/event-stream.ts";
import { callAsync, errorMessage, optional, registerCallback, releaseCallback, signalId } from "./bridge.js";

const ZERO_USAGE = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

export function failedMessage(model, text) {
	return {
		role: "assistant",
		content: [],
		api: model?.api ?? "",
		provider: model?.provider ?? "",
		model: model?.id ?? "",
		usage: ZERO_USAGE,
		stopReason: "error",
		errorMessage: text,
		timestamp: Date.now(),
	};
}

/** Options as JSON: the signal becomes a Rust token id and callbacks stay behind. */
export function streamOptionsJson(options) {
	const { signal, onPayload: _onPayload, onResponse: _onResponse, ...rest } = options ?? {};
	return { ...rest, signal: signalId(signal) };
}

/** Rust serializes assistant messages without the `role` tag. */
const withRole = (message) => (message && message.role === undefined ? { role: "assistant", ...message } : message);

function withMessageRoles(event) {
	const result = { ...event };
	for (const field of ["partial", "message", "error"]) {
		if (typeof result[field] === "object") result[field] = withRole(result[field]);
	}
	return result;
}

/** A stream fed by the Rust op `op`, called with `args`, the options, and the callback key. */
export function rustStream(model, op, args, options) {
	const stream = createAssistantMessageEventStream();
	const key = registerCallback((event) => stream.push(withMessageRoles(event)));
	callAsync(op, ...args, streamOptionsJson(options), key).then(
		(message) => {
			releaseCallback(key);
			stream.end(withRole(optional(message)));
		},
		(error) => {
			releaseCallback(key);
			const message = failedMessage(model, errorMessage(error));
			stream.push({ type: "error", reason: "error", error: message });
			stream.end(message);
		},
	);
	return stream;
}

/** `ctx.modelRegistry` streams: the session's auth and request settings apply. */
export function sessionStream(target, model, context, options) {
	return rustStream(model, "models.stream", [target, model, context], options);
}
