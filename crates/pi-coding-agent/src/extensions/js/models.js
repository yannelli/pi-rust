// `@earendil-works/pi-ai/models`: the helpers of upstream `models.ts` that need no provider
// runtime. `createModels` and `createProvider` throw; the host owns the catalog.
export { createModels, createProvider } from "./ai_extras.js";
export { ModelsError } from "../pi-ai/src/utils/models-error.ts";

const EXTENDED_THINKING_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

export const getModelType = (model) => model.type ?? "chat";

export const isModelType = (model, type) => getModelType(model) === type;

/** Non-chat models never match, even when their api id equals `api`. */
export const hasApi = (model, api) => isModelType(model, "chat") && model.api === api;

export function calculateCost(model, usage) {
	const inputTokens = usage.input + usage.cacheRead + usage.cacheWrite;
	let rates = model.cost;
	let matchedThreshold = -1;
	for (const tier of model.cost.tiers ?? []) {
		if (inputTokens > tier.inputTokensAbove && tier.inputTokensAbove > matchedThreshold) {
			rates = tier;
			matchedThreshold = tier.inputTokensAbove;
		}
	}

	// Anthropic charges 2x base input for 1h cache writes.
	const longWrite = usage.cacheWrite1h ?? 0;
	const shortWrite = usage.cacheWrite - longWrite;
	usage.cost ??= { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 };
	usage.cost.input = (rates.input / 1000000) * usage.input;
	usage.cost.output = (rates.output / 1000000) * usage.output;
	usage.cost.cacheRead = (rates.cacheRead / 1000000) * usage.cacheRead;
	usage.cost.cacheWrite = (rates.cacheWrite * shortWrite + rates.input * 2 * longWrite) / 1000000;
	usage.cost.total = usage.cost.input + usage.cost.output + usage.cost.cacheRead + usage.cost.cacheWrite;
	return usage.cost;
}

export function getSupportedThinkingLevels(model) {
	if (!model.reasoning) return ["off"];

	return EXTENDED_THINKING_LEVELS.filter((level) => {
		const mapped = model.thinkingLevelMap?.[level];
		if (mapped === null) return false;
		if (level === "xhigh" || level === "max") return mapped !== undefined;
		return true;
	});
}

export function clampThinkingLevel(model, level) {
	const availableLevels = getSupportedThinkingLevels(model);
	if (availableLevels.includes(level)) return level;

	const requestedIndex = EXTENDED_THINKING_LEVELS.indexOf(level);
	if (requestedIndex === -1) return availableLevels[0] ?? "off";

	for (let i = requestedIndex; i < EXTENDED_THINKING_LEVELS.length; i++) {
		const candidate = EXTENDED_THINKING_LEVELS[i];
		if (availableLevels.includes(candidate)) return candidate;
	}
	for (let i = requestedIndex - 1; i >= 0; i--) {
		const candidate = EXTENDED_THINKING_LEVELS[i];
		if (availableLevels.includes(candidate)) return candidate;
	}
	return availableLevels[0] ?? "off";
}

/** Whether two models share type, id, and provider. False when either is missing. */
export function modelsAreEqual(a, b) {
	if (!a || !b) return false;
	return getModelType(a) === getModelType(b) && a.id === b.id && a.provider === b.provider;
}
