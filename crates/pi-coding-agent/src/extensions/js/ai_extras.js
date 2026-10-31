// Compat members of `@earendil-works/pi-ai` outside the embedded upstream files: faux content
// helpers, and members that need the upstream faux provider, image APIs, or Bedrock module.
import { unsupportedFunction } from "./unsupported.js";

const FAUX_USAGE = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

export function fauxText(text) {
	return { type: "text", text };
}

export function fauxThinking(thinking) {
	return { type: "thinking", thinking };
}

export function fauxToolCall(name, arguments_, options = {}) {
	const id = options.id ?? `tool:${Date.now()}:${Math.random().toString(36).slice(2)}`;
	return { type: "toolCall", id, name, arguments: arguments_ };
}

export function fauxAssistantMessage(content, options = {}) {
	const blocks = typeof content === "string" ? [fauxText(content)] : Array.isArray(content) ? content : [content];
	return {
		role: "assistant",
		content: blocks,
		api: "faux",
		provider: "faux",
		model: "faux-1",
		usage: FAUX_USAGE,
		stopReason: options.stopReason ?? "stop",
		...(options.deferred === undefined ? {} : { deferred: options.deferred }),
		...(options.errorMessage === undefined ? {} : { errorMessage: options.errorMessage }),
		...(options.responseId === undefined ? {} : { responseId: options.responseId }),
		timestamp: options.timestamp ?? Date.now(),
	};
}

export const createFauxCore = unsupportedFunction("createFauxCore");
export const fauxProvider = unsupportedFunction("fauxProvider");
export const registerFauxProvider = unsupportedFunction("registerFauxProvider");
export const setBedrockProviderModule = unsupportedFunction("setBedrockProviderModule");
export const generateImages = unsupportedFunction("generateImages");
export const generateImagesOpenRouter = unsupportedFunction("generateImagesOpenRouter");
export const getImageModel = unsupportedFunction("getImageModel");
export const getImageModels = unsupportedFunction("getImageModels");
export const getImageProviders = unsupportedFunction("getImageProviders");
export const getImagesApiProvider = unsupportedFunction("getImagesApiProvider");
export const registerImagesApiProvider = unsupportedFunction("registerImagesApiProvider");
export const registerBuiltInImagesApiProviders = unsupportedFunction("registerBuiltInImagesApiProviders");
