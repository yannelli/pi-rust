// Compat image API of `@earendil-works/pi-ai` outside the embedded upstream registry: the built-in
// `openrouter-images` implementation runs in Rust, and image models come from the Rust catalog.
import { call, callAsync, optional } from "./bridge.js";
import { streamOptionsJson } from "./stream.js";
import { registerImagesApiProvider } from "../pi-ai/src/images-api-registry.ts";

function lazyLoadErrorImages(model, error) {
	return {
		api: model.api,
		provider: model.provider,
		model: model.id,
		output: [],
		stopReason: "error",
		errorMessage: error instanceof Error ? error.message : String(error),
		timestamp: Date.now(),
	};
}

/** Upstream `generateImagesOpenRouter`: only the caller's options carry auth. */
export async function generateImagesOpenRouter(model, context, options) {
	try {
		return await callAsync("ai.generateImages", model, context, streamOptionsJson(options));
	} catch (error) {
		return lazyLoadErrorImages(model, error);
	}
}

export function registerBuiltInImagesApiProviders() {
	registerImagesApiProvider({ api: "openrouter-images", generateImages: generateImagesOpenRouter });
}

registerBuiltInImagesApiProviders();

const imageModels = (provider) => call("ai.catalog", "models", provider, "image") ?? [];

/** Upstream `getImageModel`: a static read of the built-in image catalog. */
export function getImageModel(provider, modelId) {
	return optional(call("ai.catalog", "model", provider, modelId, "image"));
}

/** Upstream `getImageProviders`: built-in providers with at least one image model. */
export function getImageProviders() {
	return (call("ai.catalog", "providers") ?? []).filter((provider) => imageModels(provider).length > 0);
}

export function getImageModels(provider) {
	return imageModels(provider);
}
