// `parseArgs` from upstream `packages/coding-agent/src/cli/args.ts`. Extensions call it
// while they load, before a session exists, so it stays a pure function.

const THINKING_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

function isToolModifier(entry) {
	return typeof entry === "string" && (entry.startsWith("+") || entry.startsWith("-"));
}

function toolListError(entries) {
	const modifiers = entries.filter(isToolModifier);
	if (modifiers.length === 0) return undefined;
	if (modifiers.length < entries.length) return "tool names cannot be mixed with +name or -name entries";
	const pattern = modifiers.find((entry) => entry.includes("*"));
	if (pattern) return `+name and -name entries take exact tool names, not patterns: ${pattern}`;
	return undefined;
}

function splitList(value) {
	return value
		.split(",")
		.map((part) => part.trim())
		.filter((part) => part.length > 0);
}

export function parseArgs(args) {
	const result = {
		messages: [],
		fileArgs: [],
		unknownFlags: new Map(),
		diagnostics: [],
	};

	for (let i = 0; i < args.length; i++) {
		const arg = args[i];

		if (arg === "--") {
			for (const positional of args.slice(i + 1)) {
				if (positional.startsWith("@")) result.fileArgs.push(positional.slice(1));
				else result.messages.push(positional);
			}
			break;
		}
		if (arg === "--help" || arg === "-h") {
			result.help = true;
			continue;
		}
		if (arg === "--version" || arg === "-v") {
			result.version = true;
			continue;
		}
		if (arg === "--mode") {
			const mode = args[i + 1];
			if (mode === undefined || mode.startsWith("-")) {
				result.diagnostics.push({ type: "error", message: "--mode requires text, json, or rpc" });
				continue;
			}
			i++;
			if (mode !== "text" && mode !== "json" && mode !== "rpc") {
				result.diagnostics.push({
					type: "error",
					message: `Invalid mode "${mode}". Valid values: text, json, rpc`,
				});
				continue;
			}
			result.mode = mode;
			continue;
		}
		if (arg === "--continue" || arg === "-c") {
			result.continue = true;
			continue;
		}
		if (arg === "--resume" || arg === "-r") {
			result.resume = true;
			continue;
		}
		if (arg === "--provider" && i + 1 < args.length) {
			result.provider = args[++i];
			continue;
		}
		if (arg === "--model" && i + 1 < args.length) {
			result.model = args[++i];
			continue;
		}
		if (arg === "--api-key" && i + 1 < args.length) {
			result.apiKey = args[++i];
			continue;
		}
		if (arg === "--system-prompt" && i + 1 < args.length) {
			result.systemPrompt = args[++i];
			continue;
		}
		if (arg === "--append-system-prompt" && i + 1 < args.length) {
			result.appendSystemPrompt = result.appendSystemPrompt ?? [];
			result.appendSystemPrompt.push(args[++i]);
			continue;
		}
		if (arg === "--name" || arg === "-n") {
			if (i + 1 < args.length) result.name = args[++i];
			else result.diagnostics.push({ type: "error", message: "--name requires a value" });
			continue;
		}
		if (arg === "--no-session") {
			result.noSession = true;
			continue;
		}
		if (arg === "--session" && i + 1 < args.length) {
			result.session = args[++i];
			continue;
		}
		if (arg === "--session-id" && i + 1 < args.length) {
			result.sessionId = args[++i];
			continue;
		}
		if (arg === "--fork" && i + 1 < args.length) {
			result.fork = args[++i];
			continue;
		}
		if (arg === "--session-dir" && i + 1 < args.length) {
			result.sessionDir = args[++i];
			continue;
		}
		if (arg === "--models" && i + 1 < args.length) {
			result.models = splitList(args[++i]);
			continue;
		}
		if (arg === "--no-tools" || arg === "-nt") {
			result.noTools = true;
			continue;
		}
		if (arg === "--no-builtin-tools" || arg === "-nbt") {
			result.noBuiltinTools = true;
			continue;
		}
		if ((arg === "--tools" || arg === "-t") && i + 1 < args.length) {
			const tools = splitList(args[++i]);
			const error = toolListError(tools);
			if (error) result.diagnostics.push({ type: "error", message: `${arg}: ${error}` });
			else result.tools = tools;
			continue;
		}
		if ((arg === "--exclude-tools" || arg === "-xt") && i + 1 < args.length) {
			result.excludeTools = splitList(args[++i]);
			continue;
		}
		if (arg === "--thinking" && i + 1 < args.length) {
			const level = args[++i];
			if (THINKING_LEVELS.includes(level)) result.thinking = level;
			else {
				result.diagnostics.push({
					type: "warning",
					message: `Invalid thinking level "${level}". Valid values: ${THINKING_LEVELS.join(", ")}`,
				});
			}
			continue;
		}
		if (arg === "--print" || arg === "-p") {
			result.print = true;
			const next = args[i + 1];
			if (next !== undefined && !next.startsWith("@") && (!next.startsWith("-") || next.startsWith("---"))) {
				result.messages.push(next);
				i++;
			}
			continue;
		}
		if (arg === "--export" && i + 1 < args.length) {
			result.export = args[++i];
			continue;
		}
		if ((arg === "--extension" || arg === "-e") && i + 1 < args.length) {
			result.extensions = result.extensions ?? [];
			result.extensions.push(args[++i]);
			continue;
		}
		if (arg === "--no-extensions" || arg === "-ne") {
			result.noExtensions = true;
			continue;
		}
		if (arg === "--no-mcp") {
			result.noMcp = true;
			continue;
		}
		if (arg === "--skill" && i + 1 < args.length) {
			result.skills = result.skills ?? [];
			result.skills.push(args[++i]);
			continue;
		}
		if (arg === "--prompt-template" && i + 1 < args.length) {
			result.promptTemplates = result.promptTemplates ?? [];
			result.promptTemplates.push(args[++i]);
			continue;
		}
		if (arg === "--theme" && i + 1 < args.length) {
			result.themes = result.themes ?? [];
			result.themes.push(args[++i]);
			continue;
		}
		if (arg === "--use-theme") {
			const themeName = args[i + 1];
			if (themeName === undefined || themeName.startsWith("-")) {
				result.diagnostics.push({ type: "error", message: "--use-theme requires a theme name" });
			} else {
				result.useTheme = themeName;
				i++;
			}
			continue;
		}
		if (arg === "--no-skills" || arg === "-ns") {
			result.noSkills = true;
			continue;
		}
		if (arg === "--no-prompt-templates" || arg === "-np") {
			result.noPromptTemplates = true;
			continue;
		}
		if (arg === "--no-themes") {
			result.noThemes = true;
			continue;
		}
		if (arg === "--no-context-files" || arg === "-nc") {
			result.noContextFiles = true;
			continue;
		}
		if (arg === "--list-models") {
			if (i + 1 < args.length && !args[i + 1].startsWith("-") && !args[i + 1].startsWith("@")) {
				result.listModels = args[++i];
			} else result.listModels = true;
			continue;
		}
		if (arg === "--tui-mode") {
			const mode = args[i + 1];
			if (mode === "regular" || mode === "fullscreen") {
				result.tuiMode = mode;
				i++;
			} else if (mode === undefined || mode.startsWith("-")) {
				result.diagnostics.push({ type: "error", message: "--tui-mode requires regular or fullscreen" });
			} else {
				i++;
				result.diagnostics.push({
					type: "error",
					message: `Invalid TUI mode "${mode}". Valid values: regular, fullscreen`,
				});
			}
			continue;
		}
		if (arg === "--verbose") {
			result.verbose = true;
			continue;
		}
		if (arg === "--approve" || arg === "-a") {
			result.projectTrustOverride = true;
			continue;
		}
		if (arg === "--no-approve" || arg === "-na") {
			result.projectTrustOverride = false;
			continue;
		}
		if (arg === "--offline") {
			result.offline = true;
			continue;
		}
		if (arg.startsWith("@")) {
			result.fileArgs.push(arg.slice(1));
			continue;
		}
		if (arg.startsWith("--")) {
			const eqIndex = arg.indexOf("=");
			if (eqIndex !== -1) result.unknownFlags.set(arg.slice(2, eqIndex), arg.slice(eqIndex + 1));
			else {
				const flagName = arg.slice(2);
				const next = args[i + 1];
				if (next !== undefined && !next.startsWith("-") && !next.startsWith("@")) {
					result.unknownFlags.set(flagName, next);
					i++;
				} else result.unknownFlags.set(flagName, true);
			}
			continue;
		}
		if (arg.startsWith("-") && !arg.startsWith("--")) {
			result.diagnostics.push({ type: "error", message: `Unknown option: ${arg}` });
			continue;
		}
		if (!arg.startsWith("-")) result.messages.push(arg);
	}

	return result;
}
