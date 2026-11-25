// Upstream `createCodemodeExtension`, `createToolSearchExtension`, and `createMcpExtension` over the
// native built-ins of pi-rust: the tools they register run the native tool of the calling session,
// and the MCP factory starts the native MCP support of the session it acts on.
import { call, callAsync, registerCallback, releaseCallback, signalId } from "./bridge.js";
import { contextTarget } from "./context.js";
import { unsupportedError } from "./unsupported.js";

/** Registered inactive, as upstream; `_piNative` lets Rust run its loadout hooks natively. */
function nativeTool(name, options) {
	return {
		...call("builtins.definition", name, options),
		_piNative: { name, options },
		defaultActive: false,
		async execute(toolCallId, params, signal, onUpdate, ctx) {
			const updateKey = onUpdate ? registerCallback(onUpdate) : undefined;
			try {
				const execution = { signal: signalId(signal), updateKey };
				return await callAsync("builtins.execute", contextTarget(ctx), name, options, toolCallId, params, execution);
			} finally {
				if (updateKey !== undefined) releaseCallback(updateKey);
			}
		},
	};
}

export function createCodemodeExtension(options = {}) {
	const { mode, inlineBudget, models } = options;
	return (pi) => {
		pi.registerTool(nativeTool("codemode", { mode, inlineBudget, models }));
	};
}

export function createToolSearchExtension() {
	return (pi) => {
		pi.registerTool(nativeTool("tool_search", {}));
	};
}

const MCP_CALLBACKS = ["loadConfig", "createTransport", "credentials", "openUrl", "updateConfig"];

/** Native MCP runs for the session from `session_start` on; the `/mcp` command stays the built-in one. */
export function createMcpExtension(options = {}) {
	for (const name of MCP_CALLBACKS) {
		if (options[name] !== undefined) throw unsupportedError(`createMcpExtension.${name}`);
	}
	const { logPath, startupWaitMs } = options;
	return (pi) => {
		pi.on("session_start", async (_event, ctx) => {
			await callAsync("builtins.startMcp", contextTarget(ctx), { logPath, startupWaitMs });
		});
	};
}
