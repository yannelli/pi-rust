// `@earendil-works/pi-ai` (the compat entry point extensions load): upstream utilities run as
// embedded upstream code; streaming, the catalog, and API providers go to Rust.
import { call, optional } from "./bridge.js";
import { failedMessage, rustStream } from "./stream.js";
import { createAssistantMessageEventStream } from "../pi-ai/src/utils/event-stream.ts";

export { Type } from "typebox";
export { calculateCost, modelsAreEqual } from "./models.js";
export * from "../pi-ai/src/utils/assistant-message-frame.ts";
export * from "../pi-ai/src/utils/diagnostics.ts";
export * from "../pi-ai/src/utils/event-stream.ts";
export * from "../pi-ai/src/utils/json-parse.ts";
export * from "../pi-ai/src/utils/overflow.ts";
export * from "../pi-ai/src/utils/sanitize-unicode.ts";
export { contentText, getSystemMessageText, renderSystemMessageUpdate } from "../pi-ai/src/utils/text.ts";
export * from "../pi-ai/src/utils/transcript.ts";
export * from "../pi-ai/src/utils/typebox-helpers.ts";
export { uuidv7 } from "../pi-ai/src/utils/uuid.ts";
export * from "../pi-ai/src/utils/validation.ts";

/** API providers registered from JavaScript (`registerApiProvider`), by API id. */
export const apiProviders = new Map();

export function registerApiProvider(provider, sourceId) {
	apiProviders.set(provider.api, { provider, sourceId });
	call("ai.registerApi", provider.api);
}

export function unregisterApiProviders(sourceId) {
	for (const [api, entry] of apiProviders) {
		if (entry.sourceId === sourceId) {
			apiProviders.delete(api);
			call("ai.unregisterApi", api);
		}
	}
}

export function getApiProvider(api) {
	return apiProviders.get(api)?.provider ?? (call("ai.hasApi", api) ? builtinApi(api) : undefined);
}

export function getApiProviders() {
	return [...apiProviders.values()].map((entry) => entry.provider);
}

export function clearApiProviders() {
	for (const api of apiProviders.keys()) call("ai.unregisterApi", api);
	apiProviders.clear();
}

/** A Rust API implementation; `api` overrides `model.api`, as upstream's per-API entry points do. */
function builtinApi(api) {
	const run = (model, context, options) => {
		if (!model) return failedStream(model, "No model given");
		return rustStream(model, "ai.stream", [api, model, context], options);
	};
	return { api, stream: run, streamSimple: run };
}

function failedStream(model, text) {
	const stream = createAssistantMessageEventStream();
	const message = failedMessage(model, text);
	stream.push({ type: "error", reason: "error", error: message });
	stream.end(message);
	return stream;
}

function dispatch(method) {
	return (model, context, options) => {
		const registered = apiProviders.get(model?.api)?.provider;
		if (registered) return registered[method](model, context, options);
		return builtinApi(model?.api)[method](model, context, options);
	};
}

export const stream = dispatch("stream");
export const streamSimple = dispatch("streamSimple");
export const complete = (model, context, options) => stream(model, context, options).result();
export const completeSimple = (model, context, options) => streamSimple(model, context, options).result();

export const anthropicMessagesApi = () => builtinApi("anthropic-messages");
export const azureOpenAIResponsesApi = () => builtinApi("azure-openai-responses");
export const bedrockConverseStreamApi = () => builtinApi("bedrock-converse-stream");
export const googleGenerativeAIApi = () => builtinApi("google-generative-ai");
export const googleVertexApi = () => builtinApi("google-vertex");
export const mistralConversationsApi = () => builtinApi("mistral-conversations");
export const openAICodexResponsesApi = () => builtinApi("openai-codex-responses");
export const openAICompletionsApi = () => builtinApi("openai-completions");
export const openAIResponsesApi = () => builtinApi("openai-responses");
export const piMessagesApi = () => builtinApi("pi-messages");

export const getModel = (provider, modelId) => optional(call("ai.catalog", "model", provider, modelId));
export const getModels = (provider) => call("ai.catalog", "models", provider ?? null) ?? [];
export const getProviders = () => call("ai.catalog", "providers") ?? [];
export const getBuiltinModel = getModel;
export const getBuiltinModels = getModels;
export const getBuiltinProviders = getProviders;
export const getEnvApiKey = (provider) => optional(call("ai.envApiKey", provider));

export function supportsXhigh(model) {
	return Boolean(model?.thinkingLevelMap?.xhigh);
}
