// Upstream `Provider` objects of `pi.registerProvider(provider)` and `ModelRuntime.registerNativeProvider`.
// Rust keeps their models, credentials, and refresh state, and calls their functions here. A
// callback names the object by `target`: 0 for the `pi` runtime that registered it, else the id
// of the SDK `ModelRuntime` it was registered with.
import { defaultProviderAuthContext } from "../pi-ai/src/auth/context.ts";
import { normalizeContext } from "../pi-ai/src/utils/transcript.ts";
import { call, callAsync, optional, registerCallback, releaseCallback, signalFor, signalId } from "./bridge.js";
import { withoutNulls } from "./provider_oauth.js";
import { unsupportedError } from "./unsupported.js";

/** Provider objects of SDK `ModelRuntime`s other than the host's, by runtime id, then provider id. */
export const sdkNativeProviders = new Map();

/** The extension runtime of this host; the SDK `ModelRuntime` of the host registers through it. */
export let hostRuntime;

export function setHostRuntime(runtime) {
	hostRuntime = runtime;
}

const typeOf = (model) => model?.type ?? "chat";
const json = (value) => (value === undefined ? null : JSON.parse(JSON.stringify(value)));

/** Models of every type (upstream `Models.getAllModels`); none when the provider throws. */
export function allModels(provider) {
	try {
		return [...(provider.getAllModels?.() ?? provider.getModels())];
	} catch {
		return [];
	}
}

/** What Rust keeps of a provider object; its functions stay here. */
export function nativeProviderJson(provider) {
	const { apiKey, oauth } = provider.auth ?? {};
	return {
		name: provider.name,
		baseUrl: provider.baseUrl,
		models: json(allModels(provider)),
		apiKey: apiKey ? { login: typeof apiKey.login === "function" } : null,
		oauth: oauth ? { name: oauth.name, isSubscription: oauth.isSubscription === true, loginLabel: oauth.loginLabel } : null,
		refreshModels: typeof provider.refreshModels === "function",
		refreshModelsAuth: provider.refreshModelsAuth ?? "credential",
		filterModels: typeof provider.filterModels === "function" || typeof provider.filterAllModels === "function",
	};
}

/** The id of a provider object, with upstream's error for a blank one. */
export function nativeProviderId(provider) {
	const id = provider?.id;
	if (typeof id !== "string" || !id.trim()) throw new Error("Provider id must not be empty.");
	return id;
}

/** Upstream `registerNativeProvider` of the shared extension runtime: the objects stay here for the
 * callbacks Rust makes, and replace a provider config of the same id. */
export function nativeRuntime(runtime) {
	return {
		registerNativeProvider(provider, extensionPath) {
			if (!runtime.modelsBound) {
				runtime.pendingNativeProviders.push({ provider, extensionPath });
				return;
			}
			runtime.applyNativeProvider(provider, extensionPath);
		},
		applyNativeProvider(provider, extensionPath) {
			try {
				runtime.setNativeProvider(provider, extensionPath);
			} catch (error) {
				const message = error instanceof Error ? error.message : String(error);
				call("error", { extensionPath, event: "register_provider", error: message, stack: null });
			}
		},
		setNativeProvider(provider, extensionPath) {
			const id = nativeProviderId(provider);
			const previous = runtime.nativeProviders.get(id);
			runtime.nativeProviders.set(id, { provider, extensionPath });
			try {
				runtime.modelCall("provider.registerNative", id, nativeProviderJson(provider), extensionPath);
			} catch (error) {
				if (previous) runtime.nativeProviders.set(id, previous);
				else runtime.nativeProviders.delete(id);
				throw error;
			}
			runtime.providers.delete(id);
		},
	};
}

// Header objects `before_provider_headers` handlers saw. Upstream passes that object on to the
// provider, and extensions key per-request state by it, so a provider gets it back.
const recentHeaders = [];

export function rememberHeaders(headers) {
	if (!headers || typeof headers !== "object") return;
	recentHeaders.push(headers);
	if (recentHeaders.length > 16) recentHeaders.shift();
}

const headerList = (headers) => JSON.stringify(Object.entries(headers).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));

function requestHeaders(headers) {
	if (!headers) return headers;
	const wanted = headerList(headers);
	const index = recentHeaders.findIndex((each) => headerList(each) === wanted);
	return index === -1 ? headers : recentHeaders.splice(index, 1)[0];
}

/** The model the provider listed, with the fields Rust resolved (such as the auth `baseUrl`) over it. */
function providerModel(provider, model) {
	const listed = allModels(provider).find((each) => each.id === model.id && typeOf(each) === typeOf(model));
	return listed ? { ...listed, ...model } : model;
}

const abortable = (id) => signalFor(id) ?? new AbortController().signal;

/** Upstream `AuthInteraction` over the Rust login `keys`; prompts carry their own signal as an id. */
function loginInteraction(keys) {
	return {
		signal: abortable(keys.signal),
		prompt: ({ signal, ...prompt }) => callAsync("oauth.callback", keys, "prompt", json(prompt), signalId(signal) ?? null),
		notify: (event) => void call("oauth.notify", keys, "notify", json(event)),
	};
}

/** Upstream `getAvailable`/`getAllAvailable` filtering: `(type, id)` of the models kept. */
function filtered(provider, credential) {
	const models = allModels(provider);
	const chat = provider.filterModels
		? new Set(provider.filterModels(provider.getModels(), credential).map((model) => model.id))
		: undefined;
	const key = (model) => `${typeOf(model)}\0${model.id}`;
	const all = provider.filterAllModels ? new Set(provider.filterAllModels(models, credential).map(key)) : undefined;
	return models
		.filter((model) => (typeOf(model) === "chat" && chat ? chat.has(model.id) : !all || all.has(key(model))))
		.map((model) => [typeOf(model), model.id]);
}

/** The callbacks Rust calls for the provider objects of `runtime` and of SDK runtimes. */
export function nativeCallbacks(runtime) {
	const lookup = (target, id) => {
		const provider = target === 0 ? runtime.nativeProviders.get(id)?.provider : sdkNativeProviders.get(target)?.get(id);
		if (!provider) throw new Error(`Unknown provider: ${id}`);
		return provider;
	};
	return {
		/** Streams `streamSimple`; false when the stream ended without `done` or `error`. */
		async nativeStream(target, id, model, context, options, sink) {
			const provider = lookup(target, id);
			const signal = signalFor(options.signal);
			const request = { ...options, headers: requestHeaders(options.headers), signal };
			const stream = provider.streamSimple(providerModel(provider, model), normalizeContext(context), request);
			let ended = false;
			for await (const event of stream) {
				call("stream.event", sink, event);
				if (event.type === "done" || event.type === "error") ended = true;
			}
			return ended;
		},
		/** Upstream `RefreshModelsContext`; `publish` persists through Rust, then runs `update`. */
		async nativeRefresh(target, id, context, signalKey) {
			const provider = lookup(target, id);
			const signal = abortable(signalKey);
			await provider.refreshModels({
				credential: optional(context.credential),
				stored: optional(context.stored),
				publish: async (publication) => {
					if (signal.aborted) return false;
					const persist = publication?.persist !== undefined;
					const entry = persist ? json(publication.persist) : null;
					if (!(await callAsync("provider.publish", context.publish, persist, entry, signalKey)) || signal.aborted) return false;
					publication?.update?.();
					call("provider.nativeModels", context.publish, json(allModels(provider)));
					return true;
				},
				allowNetwork: context.allowNetwork,
				force: optional(context.force),
				signal,
				baseUrl: optional(context.baseUrl),
			});
			return json(allModels(provider));
		},
		async nativeFilter(target, id, credential) {
			return filtered(lookup(target, id), optional(credential));
		},
		/** `auth.apiKey.resolve`, or `check` (with `resolve` as upstream's fallback). */
		async nativeAuth(target, id, method, credential, signalKey) {
			const apiKey = lookup(target, id).auth?.apiKey;
			if (!apiKey) return null;
			const input = { ctx: defaultProviderAuthContext(), credential: optional(credential), signal: abortable(signalKey) };
			if (method === "check" && apiKey.check) return json(await apiKey.check(input));
			const resolved = await apiKey.resolve(input);
			if (method === "check") return resolved ? { source: resolved.source, type: "api_key" } : null;
			return json(resolved);
		},
		async nativeLogin(target, id, kind, keys, options) {
			const provider = lookup(target, id);
			const method = kind === "oauth" ? provider.auth?.oauth : provider.auth?.apiKey;
			if (!method?.login) throw new Error(`${provider.name} does not support ${kind} login`);
			const loginOptions = { agentName: optional(options.agentName) };
			if (typeof options.deviceKey === "number") loginOptions.getDeviceId = () => call("provider.deviceId", options.deviceKey);
			return json(await method.login(loginInteraction(keys), loginOptions));
		},
		async nativeOAuth(target, id, method, credential, signalKey) {
			const oauth = lookup(target, id).auth?.oauth;
			if (!oauth) throw new Error(`Provider ${id} has no OAuth auth`);
			if (method === "refresh") return json(await oauth.refresh(credential, abortable(signalKey)));
			return json(await oauth.toAuth(credential));
		},
		async nativeImages(target, id, model, context, options, signalKey) {
			const provider = lookup(target, id);
			if (!provider.generateImages) throw new Error(`Provider ${id} does not support image generation`);
			const request = { ...options, signal: signalFor(signalKey) };
			return json(await provider.generateImages(providerModel(provider, model), context, request));
		},
		async nativeClassify(target, id, model, context, options, signalKey) {
			const provider = lookup(target, id);
			if (!provider.classify) throw new Error(`Provider ${id} does not support classification`);
			const request = { ...options, signal: signalFor(signalKey) };
			return json(await provider.classify(providerModel(provider, model), context, request));
		},
	};
}

/**
 * Provider registration of SDK `ModelRuntime` `id` (upstream `ModelRuntime` and `ModelRegistry`).
 * The host's runtime (0) registers through this host's extension runtime; `base` reads Rust.
 */
export function sdkProviderMethods(id, base) {
	const natives = () => sdkNativeProviders.get(id) ?? sdkNativeProviders.set(id, new Map()).get(id);
	const nativeOf = (name) => (id === 0 ? hostRuntime.nativeProviders.get(name)?.provider : natives().get(name));
	const registerNative = (provider) => {
		if (id === 0) return hostRuntime.setNativeProvider(provider, "ModelRuntime");
		const providerId = nativeProviderId(provider);
		const previous = natives().get(providerId);
		natives().set(providerId, provider);
		try {
			call("modelRuntime.registerNative", id, providerId, nativeProviderJson(provider));
		} catch (error) {
			if (previous) natives().set(providerId, previous);
			else natives().delete(providerId);
			throw error;
		}
	};
	return {
		registerNativeProvider: registerNative,
		registerProvider(providerOrName, config) {
			if (typeof providerOrName !== "string") return registerNative(providerOrName);
			if (!config) throw new Error("Provider config is required when registering by name");
			if (id !== 0) throw unsupportedError("ModelRuntime.registerProvider(name, config)");
			hostRuntime.setProvider(providerOrName, config, "ModelRuntime");
		},
		unregisterProvider(name) {
			if (id === 0) return hostRuntime.unregisterProvider(name);
			natives().delete(name);
			call("modelRuntime.unregisterNative", id, name);
		},
		getRegisteredNativeProvider: nativeOf,
		getRegisteredProviderConfig: (name) => (id === 0 ? hostRuntime.providers.get(name)?.config : undefined),
		getRegisteredProviderIds: () => (id === 0 ? base.getRegisteredProviderIds() : [...natives().keys()]),
		getProvider: (name) => nativeOf(name) ?? base.getProvider(name),
	};
}

/** Upstream `ModelRuntime.login` and `logout` of SDK runtime `id`, run by the Rust login. */
export function sdkLoginMethods(id) {
	return {
		async login(providerId, type, interaction, options) {
			const key = registerCallback((method, payload) =>
				method === "prompt" ? interaction.prompt(withoutNulls(payload)) : interaction.notify(withoutNulls(payload)),
			);
			try {
				const loginOptions = { agentName: options?.agentName, signal: signalId(interaction.signal) };
				return await callAsync("modelRuntime.login", id, providerId, type, key, loginOptions);
			} finally {
				releaseCallback(key);
			}
		},
		async logout(providerId) {
			await callAsync("modelRuntime.logout", id, providerId);
		},
	};
}
