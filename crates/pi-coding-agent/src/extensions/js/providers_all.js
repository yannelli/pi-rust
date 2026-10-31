// `@earendil-works/pi-ai/providers/all`: upstream's catalog reads and provider factories over the
// Rust catalog. Streams, classifiers, images, API key auth, and OAuth flows run in Rust.
import { call, callAsync, optional, registerCallback, releaseCallback, signalId } from "./bridge.js";
import { BUILTIN_PROVIDERS } from "./builtin_providers.js";
import { rustStream, streamOptionsJson } from "./stream.js";
import { envApiKeyAuth } from "../pi-ai/src/auth/helpers.ts";
import { createModels, createProvider } from "../pi-ai/src/models.ts";
import {
	DEFAULT_RADIUS_GATEWAY,
	getRadiusModels,
	getRadiusModelsFromConfig,
	loadRadiusGatewayConfig,
	normalizeRadiusGatewayUrl,
} from "../pi-ai/src/providers/radius-config.ts";

const catalogModel = (type) => (provider, modelId) => optional(call("ai.catalog", "model", provider, modelId, type));
const catalogModels = (type) => (provider) =>
	provider === undefined || provider === null ? [] : (call("ai.catalog", "models", provider, type) ?? []);

export const getBuiltinModel = catalogModel("chat");
export const getBuiltinImageModel = catalogModel("image");
export const getBuiltinClassifierModel = catalogModel("classifier");
export const getBuiltinModels = catalogModels("chat");
export const getBuiltinImageModels = catalogModels("image");
export const getBuiltinClassifierModels = catalogModels("classifier");

export function getBuiltinProviders() {
	return call("ai.catalog", "providers") ?? [];
}

/** Generation timestamp shared by all built-in provider catalogs. */
export function getBuiltinModelDataGeneratedAt() {
	const generatedAt = Date.parse(call("ai.catalog", "generatedAt") ?? "");
	return Number.isNaN(generatedAt) ? undefined : generatedAt;
}

export function getAllBuiltinModels(provider) {
	return [...getBuiltinModels(provider), ...getBuiltinImageModels(provider), ...getBuiltinClassifierModels(provider)];
}

/** JSON `null` fields dropped, as upstream leaves optional fields out. */
function withoutNulls(value) {
	if (!value || typeof value !== "object") return optional(value);
	return Object.fromEntries(Object.entries(value).filter(([, entry]) => entry !== null));
}

const rustStreams = (api) => {
	const run = (model, context, options) => rustStream(model, "ai.stream", [api, model, context], options);
	return { stream: run, streamSimple: run };
};
const rustImages = { generateImages: (model, context, options) => callAsync("ai.generateImages", model, context, streamOptionsJson(options)) };
const rustClassifier = { classify: (model, context, options) => callAsync("ai.classify", model, context, streamOptionsJson(options)) };

/** Operation maps keyed by the `api` ids of `models`, per model type. */
function operations(models) {
	const apis = (type) => [...new Set(models.filter((model) => (model.type ?? "chat") === type).map((model) => model.api))];
	return {
		api: Object.fromEntries(apis("chat").map((api) => [api, rustStreams(api)])),
		images: Object.fromEntries(apis("image").map((api) => [api, rustImages])),
		classifiers: Object.fromEntries(apis("classifier").map((api) => [api, rustClassifier])),
	};
}

let credentialEnvVars;

/** Upstream `VERTEX_ADC_PATH`, which the caller's `fileExists` resolves. */
const VERTEX_ADC_PATH = "~/.config/gcloud/application_default_credentials.json";

/** The credential variables and files Rust resolution reads, as `ctx` reports them. */
async function contextEnv(provider, ctx, credential, signal) {
	credentialEnvVars ??= call("ai.credentialEnvVars");
	const vars = {};
	for (const name of credentialEnvVars) {
		const value = await ctx.env(name);
		signal?.throwIfAborted();
		if (value) vars[name] = value;
	}
	const files = {};
	if (provider === "google-vertex") {
		const adcPath = credential?.env?.GOOGLE_APPLICATION_CREDENTIALS ?? vars.GOOGLE_APPLICATION_CREDENTIALS ?? VERTEX_ADC_PATH;
		files[adcPath] = await ctx.fileExists(adcPath);
		signal?.throwIfAborted();
	}
	return { vars, files };
}

/** An `ApiKeyAuth` named `name` that resolves as the Rust resolver does for `provider`, over `ctx`. */
function rustApiKeyAuth(provider, name) {
	return {
		name,
		login: envApiKeyAuth(name, []).login,
		resolve: async ({ ctx, credential, signal }) => {
			signal?.throwIfAborted();
			const context = credential?.key ? {} : await contextEnv(provider, ctx, credential, signal);
			const result = await callAsync("ai.apiKey", provider, credential ?? null, context);
			signal?.throwIfAborted();
			return result ? withoutNulls({ ...result, auth: withoutNulls(result.auth) }) : undefined;
		},
	};
}

const withoutType = ({ type: _type, ...credential }) => credential;

/** The Rust OAuth flow of `target` (`{ provider, name?, gateway? }`) with upstream's `OAuthAuth` shape. */
function rustOAuth(target, info) {
	return {
		name: info.name,
		isSubscription: info.isSubscription,
		loginLabel: info.loginLabel,
		async login(interaction, options) {
			const key = registerCallback((method, payload) =>
				method === "prompt" ? interaction.prompt(withoutNulls(payload)) : interaction.notify(withoutNulls(payload)),
			);
			try {
				const loginOptions = { agentName: options?.agentName, signal: signalId(interaction.signal) };
				return { type: "oauth", ...(await callAsync("ai.oauth", "login", target, key, loginOptions)) };
			} finally {
				releaseCallback(key);
			}
		},
		async refresh(credential, signal) {
			const refreshed = await callAsync("ai.oauth", "refresh", target, withoutType(credential), signalId(signal) ?? null);
			return { type: "oauth", ...refreshed };
		},
		toAuth: async (credential) => withoutNulls(await callAsync("ai.oauth", "toAuth", target, withoutType(credential))),
	};
}

function builtinProvider(entry) {
	const models = getAllBuiltinModels(entry.id);
	const auth = {};
	if (entry.apiKey) auth.apiKey = rustApiKeyAuth(entry.id, entry.apiKey);
	if (entry.oauth) auth.oauth = rustOAuth({ provider: entry.id }, entry.oauth);
	return createProvider({ id: entry.id, name: entry.name, baseUrl: entry.baseUrl, headers: entry.headers, auth, models, ...operations(models) });
}

/** Radius gateway provider with a persisted, dynamically refreshed catalog (upstream `radius.ts`). */
export function radiusProvider(options = {}) {
	const id = options.id ?? "radius";
	const name = options.name ?? "Radius";
	const gateway = normalizeRadiusGatewayUrl(options.gateway ?? DEFAULT_RADIUS_GATEWAY);
	const baselineModels =
		gateway === normalizeRadiusGatewayUrl(DEFAULT_RADIUS_GATEWAY)
			? getBuiltinModels("radius").map((model) => ({ ...model, provider: id }))
			: [];
	// Once known, the gateway catalog of the account replaces the shipped baseline.
	let dynamicModels;
	const streams = rustStreams("pi-messages");

	return {
		id,
		name,
		auth: {
			apiKey: envApiKeyAuth("Radius API key", ["RADIUS_API_KEY"]),
			oauth: rustOAuth({ provider: "radius", name, gateway }, { name }),
		},
		getModels: () => dynamicModels ?? baselineModels,
		refreshModels: async (context) => {
			const stored = context.stored;
			if (stored) {
				const restored = stored.models.filter((model) => model.provider === id);
				if (!(await context.publish({ update: () => (dynamicModels = restored) }))) return;
			}
			if (!stored && context.credential?.type === "oauth") {
				const legacy = getRadiusModels(id, context.credential);
				if (legacy.length > 0) {
					const persist = { models: legacy, checkedAt: Date.now() };
					if (!(await context.publish({ persist, update: () => (dynamicModels = legacy) }))) return;
				}
			}
			if (!context.allowNetwork || context.signal.aborted) return;
			const apiKey = context.credential?.type === "oauth" ? context.credential.access : context.credential?.key;
			const config = await loadRadiusGatewayConfig(gateway, apiKey, context.signal);
			if (context.signal.aborted) return;
			const refreshed = getRadiusModelsFromConfig(id, config);
			await context.publish({ persist: { models: refreshed, checkedAt: Date.now() }, update: () => (dynamicModels = refreshed) });
		},
		stream: (model, context, streamOptions) => streams.stream(model, context, streamOptions),
		streamSimple: (model, context, streamOptions) => streams.streamSimple(model, context, streamOptions),
	};
}

/** All built-in providers, freshly constructed. */
export function builtinProviders() {
	return BUILTIN_PROVIDERS.map((entry) => (entry.id === "radius" ? radiusProvider() : builtinProvider(entry)));
}

/** A `Models` collection with every built-in provider registered. */
export function builtinModels(options) {
	const models = createModels(options);
	for (const provider of builtinProviders()) {
		models.setProvider(provider);
	}
	return models;
}
