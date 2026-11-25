// SDK `createAgentSession({ customTools })`: each definition stays in this host and runs here when
// the child session calls it (`host.sdkTool`), with a context of that session.
import { call, signalFor } from "./bridge.js";
import { createToolContext } from "./context.js";
import { toolDescriptor } from "./host.js";

const definitions = new Map();
let nextKey = 1;

const normalize = (result) =>
	result && typeof result === "object" ? { ...result, content: Array.isArray(result.content) ? result.content : [] } : { content: [] };

/** Keep `tools` for one child session; `release()` drops them when it is disposed. */
export function registerCustomTools(tools) {
	if (!Array.isArray(tools)) throw new TypeError("createAgentSession.customTools must be an array");
	if (tools.some((definition) => typeof definition?.name !== "string" || typeof definition.execute !== "function")) {
		throw new TypeError("Each createAgentSession.customTools entry needs a name and an execute function");
	}
	const keys = tools.map((definition) => {
		definitions.set(nextKey, definition);
		return nextKey++;
	});
	const descriptors = tools.map((definition, index) => ({
		...toolDescriptor(definition, undefined, `<sdk:${definition.name}>`),
		key: keys[index],
	}));
	return { descriptors, release: () => keys.forEach((key) => definitions.delete(key)) };
}

/** A call of the child session `target` to custom tool `key`, as the host methods of named tools take it. */
export function customToolCall(runner, key, target, method, ...args) {
	const tool = definitions.get(key);
	if (!tool) throw new Error("The session of this custom tool has been disposed");
	if (method === "executeTool") {
		const [toolCallId, params, signal, updateKey] = args;
		const abort = signalFor(signal);
		const update = (partial) => call("tool.update", updateKey, normalize(partial));
		const onUpdate = updateKey ? Object.assign(update, { updateKey }) : undefined;
		const ctx = createToolContext(runner, toolCallId, abort, target);
		return Promise.resolve(tool.execute(toolCallId, params, abort, onUpdate, ctx)).then(normalize);
	}
	const [input] = args;
	if (method === "prepareArguments") return tool.prepareArguments?.(input) ?? input;
	if (method === "reachesTools") return tool.reachesTools?.(input) ?? [];
	const view = {
		...input,
		getExposure: (name) => input.exposures?.[name] ?? "direct",
		getNamespace: (name) => input.namespaces?.[name],
		getPromptGuidelines: (name) => input.guidelines?.[name] ?? [],
	};
	return tool.prepareLoadout?.(view) ?? null;
}
