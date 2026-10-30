// `@earendil-works/pi-agent-core`: the upstream agent and loop run as embedded upstream code.
// Like upstream `sdk.ts`, the default stream function is pi-ai's `streamSimple`.
import { streamSimple } from "./ai.js";
import { setDefaultStreamFn } from "../pi-agent-core/src/stream-fn.ts";

export * from "../pi-agent-core/src/index.ts";
export { setDefaultStreamFn };

setDefaultStreamFn(streamSimple);
