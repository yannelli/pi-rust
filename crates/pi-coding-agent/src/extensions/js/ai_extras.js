// Compat members of `@earendil-works/pi-ai` outside the embedded upstream files: the faux provider
// registered with the host API registry, the image API, and the Bedrock module hook.
import { registerApiProvider, unregisterApiProviders } from "./ai.js";
import { unsupportedFunction } from "./unsupported.js";
import { createFauxCore } from "../pi-ai/src/providers/faux.ts";

export {
	createFauxCore,
	fauxAssistantMessage,
	fauxProvider,
	fauxText,
	fauxThinking,
	fauxToolCall,
} from "../pi-ai/src/providers/faux.ts";
export { generateImages } from "../pi-ai/src/images.ts";
export { getImagesApiProvider, registerImagesApiProvider } from "../pi-ai/src/images-api-registry.ts";
export {
	generateImagesOpenRouter,
	getImageModel,
	getImageModels,
	getImageProviders,
	registerBuiltInImagesApiProviders,
} from "./ai_images.js";

/** Upstream compat `registerFauxProvider`: a faux core registered as a JavaScript API provider. */
export function registerFauxProvider(options = {}) {
	const core = createFauxCore(options);
	const sourceId = `faux-provider-${Math.random().toString(36).slice(2, 10)}`;
	registerApiProvider({ api: core.api, stream: core.stream, streamSimple: core.streamSimple }, sourceId);
	return {
		api: core.api,
		models: core.models,
		getModel: core.getModel,
		state: core.state,
		setResponses: core.setResponses,
		appendResponses: core.appendResponses,
		getPendingResponseCount: core.getPendingResponseCount,
		unregister() {
			unregisterApiProviders(sourceId);
		},
	};
}

export const setBedrockProviderModule = unsupportedFunction("setBedrockProviderModule");
