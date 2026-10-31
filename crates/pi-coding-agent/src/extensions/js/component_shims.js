// Host versions of upstream modules that vendored interactive components import: the agent
// directory, processes, keybindings, clipboard, external editor, and images come from Rust.
import { getCapabilities, setImageTranscoder } from "@earendil-works/pi-tui";
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

/** Upstream `modes/interactive/external-editor.ts`: Rust runs the editor in the terminal. */
export const editInExternalEditor = ({ command, content }) => callAsync("ui.externalEditor", 0, command, content);

let pngTranscoderRegistered = false;

/** Upstream `utils/image-convert.ts`: Kitty terminals get PNG through the Rust image converter. */
export function ensurePngTranscoder(onRegistered) {
	if (pngTranscoderRegistered || getCapabilities().images !== "kitty") return;
	setImageTranscoder((data, mimeType) => call("images.convertToPng", data, mimeType)?.data ?? null);
	pngTranscoderRegistered = true;
	onRegistered();
}
