// Host versions of upstream modules that vendored interactive components import: the agent
// directory, process spawning, app keybindings, and the clipboard come from the Rust host.
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { call, callAsync } from "./bridge.js";
import { appKeybindings } from "./ui.js";

/** Upstream `config.ts`. */
export const getAgentDir = () => call("config.info").agentDir;
export const getBinDir = () => join(getAgentDir(), "bin");

/** Upstream `utils/child-process.ts`; its callers here run only macOS and Linux tools. */
export const spawnProcessSync = (command, args, options) => spawnSync(command, args, options);

/** Upstream `core/keybindings.ts`: `create()` returns the app keybindings, `keybindings.json` included. */
export const KeybindingsManager = { create: appKeybindings };

/** Upstream `utils/clipboard.ts`, through the Rust clipboard. */
export const copyToClipboard = (text) => callAsync("clipboard.copy", text).then(() => undefined);
