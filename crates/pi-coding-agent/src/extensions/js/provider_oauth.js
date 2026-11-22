// Provider OAuth methods backed by Rust, for session registries and built-in SDK providers.
import { callAsync, optional, registerCallback, releaseCallback, signalId } from "./bridge.js";

/** JSON `null` fields dropped, as upstream leaves optional fields out. */
export function withoutNulls(value) {
	if (!value || typeof value !== "object") return optional(value);
	return Object.fromEntries(Object.entries(value).filter(([, entry]) => entry !== null));
}

const withoutType = ({ type: _type, ...credential }) => credential;

/** The Rust OAuth flow of `target` (`{ provider, name?, gateway? }`) with upstream's `OAuthAuth` shape. */
export function rustOAuth(target, info, assertActive) {
	return {
		name: info.name,
		isSubscription: info.isSubscription,
		loginLabel: info.loginLabel,
		async login(interaction, options) {
			assertActive?.();
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
			assertActive?.();
			const refreshed = await callAsync("ai.oauth", "refresh", target, withoutType(credential), signalId(signal) ?? null);
			return { type: "oauth", ...refreshed };
		},
		async toAuth(credential) {
			assertActive?.();
			return withoutNulls(await callAsync("ai.oauth", "toAuth", target, withoutType(credential)));
		},
	};
}
