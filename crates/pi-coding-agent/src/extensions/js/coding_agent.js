// `@earendil-works/pi-coding-agent` as extensions see it: upstream UI helpers and truncation run
// as embedded upstream code; config, messages, built-in tools, and shell access go to Rust.
import { createEventBus } from "./api.js";
import { call, callAsync, optional, registerCallback, releaseCallback, signalFor, signalId } from "./bridge.js";

export { ArminComponent } from "../pi-coding-agent/src/modes/interactive/components/armin.ts";
export { AssistantMessageComponent } from "../pi-coding-agent/src/modes/interactive/components/assistant-message.ts";
export { BashExecutionComponent } from "../pi-coding-agent/src/modes/interactive/components/bash-execution.ts";
export { BranchSummaryMessageComponent } from "../pi-coding-agent/src/modes/interactive/components/branch-summary-message.ts";
export { CompactionSummaryMessageComponent } from "../pi-coding-agent/src/modes/interactive/components/compaction-summary-message.ts";
export { CustomMessageComponent } from "../pi-coding-agent/src/modes/interactive/components/custom-message.ts";
export { ExtensionInputComponent } from "../pi-coding-agent/src/modes/interactive/components/extension-input.ts";
export { ExtensionSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/extension-selector.ts";
export { OAuthSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/oauth-selector.ts";
export { ShowImagesSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/show-images-selector.ts";
export { SkillInvocationMessageComponent } from "../pi-coding-agent/src/modes/interactive/components/skill-invocation-message.ts";
export { ThinkingSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/thinking-selector.ts";
export { UserMessageComponent } from "../pi-coding-agent/src/modes/interactive/components/user-message.ts";
export { UserMessageSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/user-message-selector.ts";
export { LoginDialogComponent } from "../pi-coding-agent/src/modes/interactive/components/login-dialog.ts";
export { SessionSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/session-selector.ts";
export { ThemeSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/theme-selector.ts";
export { TreeSelectorComponent } from "../pi-coding-agent/src/modes/interactive/components/tree-selector.ts";
export { BorderedLoader } from "../pi-coding-agent/src/modes/interactive/components/bordered-loader.ts";
export { CustomEditor } from "../pi-coding-agent/src/modes/interactive/components/custom-editor.ts";
export { DynamicBorder } from "../pi-coding-agent/src/modes/interactive/components/dynamic-border.ts";
export {
	formatKeyText,
	keyDisplayText,
	keyHint,
	keyText,
	rawKeyHint,
} from "../pi-coding-agent/src/modes/interactive/components/keybinding-hints.ts";
export { truncateToVisualLines } from "../pi-coding-agent/src/modes/interactive/components/visual-truncate.ts";
export { withFileMutationQueue } from "../pi-coding-agent/src/core/tools/file-mutation-queue.ts";
export * from "./coding_unsupported.js";
export * from "./coding_utils.js";
export * from "./compaction_utils.js";
export { createAgentSession } from "./sdk_session.js";
export { DefaultResourceLoader, ModelRegistry, ModelRuntime } from "./sdk_resources.js";
export { SessionManager, SettingsManager } from "./sdk_storage.js";
export {
	buildContextEntries,
	buildSessionContext,
	buildSessionProjection,
	getLatestCompactionEntry,
	migrateSessionEntries,
	parseSessionEntries,
	sessionEntryToContextMessages,
} from "./session_utils.js";

function parseLineNumber(value) {
	if (typeof value === "string" && value.trim() !== "") value = Number(value);
	return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/** Upstream `formatReadLineRange` from `core/tools/render-utils.ts`. */
export function formatReadLineRange(offset, limit) {
	if (offset == null && limit == null) return "";
	const start = offset == null ? 1 : parseLineNumber(offset);
	if (start === undefined) return "";
	if (limit == null) return `:${start}`;
	const count = parseLineNumber(limit);
	if (count === undefined) return "";
	const end = start + count - 1;
	return `:${start}${end ? `-${end}` : ""}`;
}
export {
	DEFAULT_MAX_BYTES,
	DEFAULT_MAX_LINES,
	formatSize,
	GREP_MAX_LINE_LENGTH,
	truncateHead,
	truncateLine,
	truncateTail,
} from "../pi-coding-agent/src/core/tools/truncate.ts";
export {
	getMarkdownTheme,
	getSelectListTheme,
	getSettingsListTheme,
	highlightCode,
	Theme,
	theme,
} from "./theme.js";
export { createEventBus };

const config = call("config.info");

export const CONFIG_DIR_NAME = config.configDirName;
export const APP_NAME = config.appName;
export const VERSION = config.version;
export const CURRENT_SESSION_VERSION = 3;
export const VIRTUAL_MODEL_STATE_ENTRY = "pi.virtual-model-state";
export const DEFAULT_COMPACTION_SETTINGS = { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 };

export const getAgentDir = () => config.agentDir;
export const getPackageDir = () => config.packageDir;
export const getDocsPath = () => `${config.packageDir}/docs`;
export const getExamplesPath = () => `${config.packageDir}/examples`;
export const getReadmePath = () => `${config.packageDir}/README.md`;
export const getLanguageFromPath = (path) => optional(call("theme.language", path));
export const initTheme = () => undefined;

export function defineTool(tool) {
	return tool;
}

export function isToolCallEventType(toolName, event) {
	return event.toolName === toolName;
}

const isResultOf = (name) => (event) => event.toolName === name;
export const isBashToolResult = isResultOf("bash");
export const isPowerShellToolResult = isResultOf("powershell");
export const isReadToolResult = isResultOf("read");
export const isEditToolResult = isResultOf("edit");
export const isWriteToolResult = isResultOf("write");
export const isGrepToolResult = isResultOf("grep");
export const isFindToolResult = isResultOf("find");
export const isLsToolResult = isResultOf("ls");

export const convertToLlm = (messages) => call("messages.convertToLlm", messages);
export const serializeConversation = (messages) => call("messages.serialize", messages);
export const estimateTokens = (message) => call("messages.estimateTokens", message);
export const parseFrontmatter = (content) => call("text.parseFrontmatter", content);
export const stripFrontmatter = (content) => call("text.parseFrontmatter", content).body;
export const copyToClipboard = (text) => callAsync("clipboard.copy", text).then(() => undefined);
export const getShellConfig = () => call("shell.config", "bash");
export const getPowerShellConfig = () => call("shell.config", "powershell");

export function calculateContextTokens(usage) {
	return usage.totalTokens || usage.input + usage.output + usage.cacheRead + usage.cacheWrite;
}

export function shouldCompact(contextTokens, contextWindow, settings) {
	if (!settings.enabled) return false;
	return contextTokens > contextWindow - settings.reserveTokens;
}

export function parseSkillBlock(text) {
	const match = text.match(/^<skill name="([^"]+)" location="([^"]+)">\n([\s\S]*?)\n<\/skill>(?:\n\n([\s\S]+))?$/);
	if (!match) return null;
	return { name: match[1], location: match[2], content: match[3], userMessage: match[4]?.trim() || undefined };
}

/** Local bash execution (upstream `createLocalBashOperations`). */
export function createLocalBashOperations(options) {
	return { exec: (command, cwd, execOptions) => runShell("bash", command, cwd, execOptions, options) };
}

export function createLocalPowerShellOperations(options) {
	return { exec: (command, cwd, execOptions) => runShell("powershell", command, cwd, execOptions, options) };
}

async function runShell(shell, command, cwd, execOptions = {}, options = {}) {
	const dataKey = execOptions.onData ? registerCallback((chunk) => execOptions.onData(Buffer.from(chunk))) : undefined;
	try {
		return await callAsync("shell.exec", shell, command, cwd, {
			dataKey,
			signal: signalId(execOptions.signal),
			timeout: execOptions.timeout,
			env: execOptions.env,
			shellPath: options.shellPath,
		});
	} finally {
		if (dataKey !== undefined) releaseCallback(dataKey);
	}
}

/** A built-in tool as a tool definition; `operations` replace how bash runs commands. */
function builtinDefinition(name, cwd, options = {}) {
	const info = call("tools.builtinInfo", name);
	const toolOptions = { commandPrefix: options.commandPrefix, shellPath: options.shellPath };
	if (options.operations) {
		if (name !== "bash" && name !== "powershell") {
			throw new Error(`Custom operations are supported for the bash and powershell tools, not ${name}`);
		}
		const operations = options.operations;
		toolOptions.operations = registerCallback(async (command, commandCwd, execOptions) => {
			const onData = execOptions.dataKey ? (data) => call("bash.data", execOptions.dataKey, String(data)) : () => {};
			const result = await operations.exec(command, commandCwd, {
				onData,
				signal: signalFor(execOptions.signal),
				timeout: execOptions.timeout,
				env: execOptions.env,
			});
			return { exitCode: result?.exitCode ?? null };
		});
	}
	if (typeof options.spawnHook === "function") toolOptions.spawnHook = registerCallback(options.spawnHook);
	return {
		...info,
		async execute(toolCallId, params, signal, onUpdate) {
			const updateKey = onUpdate ? registerCallback(onUpdate) : undefined;
			try {
				return await callAsync("tools.builtinExecute", name, cwd, toolCallId, params, {
					...toolOptions,
					signal: signalId(signal),
					updateKey,
				});
			} finally {
				if (updateKey !== undefined) releaseCallback(updateKey);
			}
		},
	};
}

function toolFactories(name) {
	const definition = (cwd, options) => builtinDefinition(name, cwd, options);
	const tool = (cwd, options) => {
		const { execute, ...rest } = builtinDefinition(name, cwd, options);
		return { ...rest, execute: (toolCallId, params, signal, onUpdate) => execute(toolCallId, params, signal, onUpdate) };
	};
	return [tool, definition];
}

export const [createReadTool, createReadToolDefinition] = toolFactories("read");
export const [createBashTool, createBashToolDefinition] = toolFactories("bash");
export const [createPowerShellTool, createPowerShellToolDefinition] = toolFactories("powershell");
export const [createEditTool, createEditToolDefinition] = toolFactories("edit");
export const [createWriteTool, createWriteToolDefinition] = toolFactories("write");
export const [createGrepTool, createGrepToolDefinition] = toolFactories("grep");
export const [createFindTool, createFindToolDefinition] = toolFactories("find");
export const [createLsTool, createLsToolDefinition] = toolFactories("ls");

export function createCodingTools(cwd, options) {
	return ["read", "bash", "edit", "write"].map((name) => toolFactories(name)[0](cwd, options?.[name]));
}

export function createReadOnlyTools(cwd, options) {
	return ["read", "grep", "find", "ls"].map((name) => toolFactories(name)[0](cwd, options?.[name]));
}
