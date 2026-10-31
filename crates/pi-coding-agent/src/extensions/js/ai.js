// `@earendil-works/pi-ai` (the compat entry point extensions load): upstream utilities run as
// embedded upstream code; streaming, the catalog, and API providers go to Rust.
import { call, optional } from "./bridge.js";
import { failedMessage, rustStream } from "./stream.js";
import { getBuiltinModel, getBuiltinModels, getBuiltinProviders } from "./providers_all.js";
import { createAssistantMessageEventStream } from "../pi-ai/src/utils/event-stream.ts";

export { Type } from "typebox";
export * from "./models.js";
export * from "./ai_extras.js";
export * from "../pi-ai/src/api/lazy.ts";
export * from "../pi-ai/src/auth/context.ts";
export * from "../pi-ai/src/auth/credential-store.ts";
export * from "../pi-ai/src/auth/helpers.ts";
export {
	ANTHROPIC_API_KEY_ENV,
	ANTHROPIC_AUTH_TOKEN_ENV,
	ANTHROPIC_FEDERATION_RULE_ID_ENV,
	ANTHROPIC_IDENTITY_TOKEN_FILE_ENV,
	ANTHROPIC_OAUTH_TOKEN_ENV,
	ANTHROPIC_ORGANIZATION_ID_ENV,
	ANTHROPIC_SERVICE_ACCOUNT_ID_ENV,
	ANTHROPIC_WORKSPACE_ID_ENV,
	findEnvKeys,
} from "../pi-ai/src/env-api-keys.ts";
export * from "../pi-ai/src/models-store.ts";
export * from "../pi-ai/src/session-resources.ts";
export * from "../pi-ai/src/utils/assistant-message-frame.ts";
export * from "../pi-ai/src/utils/diagnostics.ts";
export * from "../pi-ai/src/utils/event-stream.ts";
export * from "../pi-ai/src/utils/identity.ts";
export * from "../pi-ai/src/utils/json-parse.ts";
export * from "../pi-ai/src/utils/models-error.ts";
export * from "../pi-ai/src/utils/overflow.ts";
export * from "../pi-ai/src/utils/retry.ts";
export * from "../pi-ai/src/utils/sanitize-unicode.ts";
export * from "../pi-ai/src/utils/streaming-json.ts";
export { contentText, getSystemMessageText, renderSystemMessageUpdate } from "../pi-ai/src/utils/text.ts";
export * from "../pi-ai/src/utils/transcript.ts";
export * from "../pi-ai/src/utils/typebox-helpers.ts";
export { uuidv7 } from "../pi-ai/src/utils/uuid.ts";
export * from "../pi-ai/src/utils/validation.ts";
export * from "../pi-ai/src/utils/version.ts";

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

/** Upstream restores the built-in APIs; here they stay registered in Rust, so only JavaScript ones go. */
export const resetApiProviders = clearApiProviders;
export const registerBuiltInApiProviders = () => undefined;

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

const legacy = (api) => [(model, context, options) => builtinApi(api).stream(model, context, options),
	(model, context, options) => builtinApi(api).streamSimple(model, context, options)];
export const [streamAnthropic, streamSimpleAnthropic] = legacy("anthropic-messages");
export const [streamAzureOpenAIResponses, streamSimpleAzureOpenAIResponses] = legacy("azure-openai-responses");
export const [streamGoogle, streamSimpleGoogle] = legacy("google-generative-ai");
export const [streamGoogleVertex, streamSimpleGoogleVertex] = legacy("google-vertex");
export const [streamMistral, streamSimpleMistral] = legacy("mistral-conversations");
export const [streamOpenAICodexResponses, streamSimpleOpenAICodexResponses] = legacy("openai-codex-responses");
export const [streamOpenAICompletions, streamSimpleOpenAICompletions] = legacy("openai-completions");
export const [streamOpenAIResponses, streamSimpleOpenAIResponses] = legacy("openai-responses");

export const getModel = getBuiltinModel;
export const getModels = getBuiltinModels;
export const getProviders = getBuiltinProviders;
export const getEnvApiKey = (provider) => optional(call("ai.envApiKey", provider));

export function supportsXhigh(model) {
	return Boolean(model?.thinkingLevelMap?.xhigh);
}
