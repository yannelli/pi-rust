// Event dispatch (upstream `core/extensions/runner.ts`): handlers run in load order, failures are
// reported as extension errors, and each event combines handler results as upstream does.
import { getCurrentSystemMessage } from "../pi-ai/src/utils/transcript.ts";
import { call, callAsync, errorMessage, errorStack, optional } from "./bridge.js";
import { boundaryContext, createCommandContext, createContext, createModelRegistry, createSessionManagerView, createToolContext, linkEndedMessage } from "./context.js";
import { rememberHeaders } from "./native_provider.js";
import { plain } from "./sdk_mirror.js";
import { unsupportedError } from "./unsupported.js";

export const STALE_MESSAGE =
	"This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().";

function snapshot(extensions, type) {
	return extensions.map((ext) => ({ ext, handlers: ext.handlers.get(type)?.slice() ?? [] }));
}

function sameMessages(left, right) {
	return left.length === right.length && left.every((message, index) => message === right[index]);
}

function restoreSystemMessages(current, visible, returned) {
	if (sameMessages(returned, visible)) return current;
	const head = getCurrentSystemMessage(current);
	return head ? [head, ...returned] : returned;
}

const SESSION_BEFORE = new Set(["session_before_switch", "session_before_fork", "session_before_compact", "session_before_tree"]);

/** pi-rust: a `session_before_compact` result with only `customInstructions` keeps an earlier handler's compaction. */
function keepsCompaction(type, earlier, later) {
	return type === "session_before_compact" && earlier?.compaction !== undefined && later.compaction === undefined && later.customInstructions !== undefined;
}

function isUserBashResult(value) {
	if (typeof value !== "object" || value === null) return false;
	const hasOperations = value.operations !== undefined;
	if (hasOperations === (value.result !== undefined)) return false;
	if (hasOperations) return typeof value.operations === "object" && typeof value.operations?.exec === "function";
	const result = value.result;
	return (
		typeof result === "object" &&
		result !== null &&
		typeof result.output === "string" &&
		"exitCode" in result &&
		(result.exitCode === undefined || typeof result.exitCode === "number") &&
		typeof result.cancelled === "boolean" &&
		typeof result.truncated === "boolean" &&
		(result.fullOutputPath === undefined || typeof result.fullOutputPath === "string")
	);
}

const ELICITATION_ACTIONS = new Set(["accept", "decline", "cancel"]);

/** `{ action, content? }` of an elicitation handler, or undefined; throws on other values. */
function elicitationResult(type, value) {
	if (value === undefined || value === null) return undefined;
	const content = value?.content;
	const validContent = content === undefined || (typeof content === "object" && content !== null && !Array.isArray(content));
	if (typeof value !== "object" || !ELICITATION_ACTIONS.has(value.action) || !validContent) {
		throw new Error(`Invalid ${type} handler result: return undefined or { action: "accept" | "decline" | "cancel", content? }`);
	}
	return content === undefined ? { action: value.action } : { action: value.action, content };
}

export class Runner {
	extensions = [];
	sessionManagers = new Map();
	modelRegistries = new Map();
	staleMessage;
	promptDepth = 0;
	activePrompt;

	constructor(runtime, makeStream) {
		this.runtime = runtime;
		this.makeStream = makeStream;
	}

	assertActive() {
		if (this.staleMessage) throw new Error(this.staleMessage);
	}

	invalidate(message = STALE_MESSAGE) {
		if (this.staleMessage) return;
		this.staleMessage = message;
		this.runtime.invalidate(message);
	}

	emitError(error) {
		call("error", {
			extensionPath: error.extensionPath,
			event: error.event,
			error: error.error,
			stack: error.stack ?? null,
		});
	}

	reportFailure(ext, event, error) {
		this.emitError({ extensionPath: ext.path, event, error: errorMessage(error), stack: errorStack(error) });
	}

	hasHandlers(type) {
		return this.extensions.some((ext) => (ext.handlers.get(type)?.length ?? 0) > 0);
	}

	handlerTypes() {
		const types = new Set();
		for (const ext of this.extensions) {
			for (const [type, handlers] of ext.handlers) if (handlers.length > 0) types.add(type);
		}
		return [...types];
	}

	assertTargetActive(target) {
		if (target === this.runtime.target) this.assertActive();
		if (target !== 0) call("ctx.assertActive", target);
	}

	sessionManagerFor(target) {
		if (!this.sessionManagers.has(target)) {
			this.sessionManagers.set(target, createSessionManagerView(target, () => this.assertTargetActive(target)));
		}
		return this.sessionManagers.get(target);
	}

	modelRegistryFor(target) {
		if (!this.modelRegistries.has(target)) {
			const registry = createModelRegistry(target, this.makeStream, "models", () => this.assertTargetActive(target));
			const key = async (method, ...args) => {
				this.assertTargetActive(target);
				await callAsync(`models.${method}`, target, ...args);
			};
			const provider = (method, name, config) => {
				this.assertTargetActive(target);
				if (target === this.runtime.target) return this.runtime.contextProvider(method, name, config);
				// Provider objects stay in the host of the session that registered them.
				if (method === "getNative") return undefined;
				if (typeof name !== "string") throw unsupportedError("ModelRegistry.registerProvider(provider) of a replaced session");
				return optional(call("models.contextProvider", target, method, name, plain(config, method)));
			};
			const getProvider = registry.getProvider;
			registry.getProvider = (name) => provider("getNative", name) ?? getProvider(name);
			registry.getRegisteredNativeProvider = (name) => provider("getNative", name);
			registry.runtime = {
				setRuntimeApiKey: (name, value) => key("setRuntimeApiKey", name, value),
				removeRuntimeApiKey: (name) => key("removeRuntimeApiKey", name),
			};
			registry.getRegisteredProviderConfig = (name) => provider("get", name);
			registry.registerProvider = (name, config) => provider("register", name, config);
			registry.unregisterProvider = (name) => provider("unregister", name);
			this.modelRegistries.set(target, registry);
		}
		return this.modelRegistries.get(target);
	}

	createContext() {
		return createContext(this);
	}

	/** pi-rust: `ctx.switchSession()` of `session_start` queues the switch; it runs after the handlers. */
	createStartContext() {
		const ctx = this.createContext();
		ctx.switchSession = async (sessionPath) => {
			call("session.queueSwitch", 0, String(sessionPath));
		};
		return ctx;
	}

	createToolContext(toolCallId, signal) {
		return createToolContext(this, toolCallId, signal);
	}

	createCommandContext() {
		return createCommandContext(this);
	}

	withUIPrompt(kind, title, run) {
		const outer = this.promptDepth++ === 0;
		if (outer) {
			this.activePrompt = { kind, title };
			this.emitPromptEvent("ui_prompt_start", kind, title);
		}
		const finish = () => {
			if (--this.promptDepth > 0) return;
			this.promptDepth = 0;
			const prompt = this.activePrompt ?? { kind, title };
			this.activePrompt = undefined;
			this.emitPromptEvent("ui_prompt_end", prompt.kind, prompt.title);
		};
		try {
			return run().finally(finish);
		} catch (error) {
			finish();
			throw error;
		}
	}

	emitPromptEvent(type, kind, title) {
		queueMicrotask(() => {
			void this.emit({ type, reason: "ui_prompt", kind, ...(title ? { title } : {}) });
		});
	}

	async emit(event) {
		const ctx = event.type === "session_start" ? this.createStartContext() : this.createContext();
		let result;
		for (const { ext, handlers } of snapshot(this.extensions, event.type)) {
			for (const handler of handlers) {
				try {
					const handlerResult = await handler(event, ctx);
					if (SESSION_BEFORE.has(event.type) && handlerResult) {
						result = keepsCompaction(event.type, result, handlerResult) ? { ...handlerResult, compaction: result.compaction } : handlerResult;
						if (result.cancel) return result;
					}
				} catch (error) {
					this.reportFailure(ext, event.type, error);
				}
			}
		}
		return result;
	}

	async emitCacheWarmingDecision(event) {
		const ctx = this.createContext();
		let action = event.action;
		for (const { ext, handlers } of snapshot(this.extensions, event.type)) {
			for (const handler of handlers) {
				try {
					const result = await handler(event, ctx);
					if (result?.action !== undefined) action = result.action;
				} catch (error) {
					this.reportFailure(ext, event.type, error);
				}
			}
		}
		return action;
	}

	/** pi-rust `message_display`: each handler sees the text the previous one returned. */
	async emitMessageDisplay(event) {
		const ctx = this.createContext();
		let current;
		for (const { ext, handlers } of snapshot(this.extensions, "message_display")) {
			for (const handler of handlers) {
				try {
					const text = current ?? event.text;
					const delta = current ?? event.delta;
					const result = await handler({ ...event, delta, text }, ctx);
					if (typeof result?.text === "string") current = result.text;
				} catch (error) {
					this.reportFailure(ext, "message_display", error);
				}
			}
		}
		return current;
	}

	/** `entryId`: the entry the message gets, which then holds `event.message` as upstream's does. */
	async emitMessageEnd(event, entryId) {
		const ctx = this.createContext();
		const message = event.message;
		if (entryId) linkEndedMessage(entryId, message);
		let current = message;
		let modified = false;
		for (const { ext, handlers } of snapshot(this.extensions, "message_end")) {
			for (const handler of handlers) {
				try {
					const result = await handler({ ...event, message: current }, ctx);
					if (!result?.message) continue;
					if (result.message.role !== current.role) {
						this.emitError({
							extensionPath: ext.path,
							event: "message_end",
							error: "message_end handlers must return a message with the same role",
						});
						continue;
					}
					current = result.message;
					modified = true;
				} catch (error) {
					this.reportFailure(ext, "message_end", error);
				}
			}
		}
		if (!modified) return undefined;
		const role = current.role;
		const needsContent = role === "user" || role === "assistant" || role === "toolResult" || role === "custom";
		const replacement = needsContent && current.content == null ? { ...current, content: [] } : current;
		// Upstream replaces the message in place, so the object handlers saw first stays current.
		if (replacement !== message) {
			for (const key of Object.keys(message)) delete message[key];
			Object.assign(message, replacement);
		}
		return replacement;
	}

	async emitToolResult(event) {
		const ctx = this.createContext();
		const current = { ...event };
		let modified = false;
		for (const { ext, handlers } of snapshot(this.extensions, "tool_result")) {
			for (const handler of handlers) {
				try {
					const result = await handler(current, ctx);
					if (!result) continue;
					if (result.content !== undefined) {
						current.content = result.content;
						if (result.structuredContent === undefined) delete current.structuredContent;
						modified = true;
					}
					for (const key of ["details", "structuredContent", "isError", "usage"]) {
						if (result[key] !== undefined) {
							current[key] = result[key];
							modified = true;
						}
					}
				} catch (error) {
					this.reportFailure(ext, "tool_result", error);
				}
			}
		}
		if (!modified) return undefined;
		const { content, details, structuredContent, isError, usage } = current;
		return { content, details, structuredContent, isError, usage };
	}

	/** Failures propagate: a throwing handler blocks the call. `input` carries in-place edits. */
	async emitToolCall(event) {
		const ctx = this.createContext();
		let result;
		for (const { ext, handlers } of snapshot(this.extensions, "tool_call")) {
			for (const handler of handlers) {
				const handlerResult = await handler(event, ctx);
				if (handlerResult) {
					result = handlerResult;
					if (result.block) return { block: true, reason: result.reason, input: event.input, extensionPath: ext.path };
				}
			}
		}
		return { block: false, reason: result?.reason, input: event.input };
	}

	async emitUserBash(event) {
		const ctx = this.createContext();
		for (const { ext, handlers } of snapshot(this.extensions, "user_bash")) {
			for (const handler of handlers) {
				try {
					const result = await handler(event, ctx);
					if (result === undefined) continue;
					if (!isUserBashResult(result)) {
						throw new Error(
							"Invalid user_bash handler result: return undefined for local execution or exactly one valid { operations } or { result } object",
						);
					}
					return result;
				} catch (error) {
					this.reportFailure(ext, "user_bash", error);
					throw error;
				}
			}
		}
		return undefined;
	}

	/** pi-rust: the first answer of the handlers, or the first decline. */
	async emitMcpElicitation(event) {
		const ctx = this.createContext();
		let answer;
		for (const { ext, handlers } of snapshot(this.extensions, "mcp_elicitation")) {
			for (const handler of handlers) {
				try {
					const result = elicitationResult("mcp_elicitation", await handler(event, ctx));
					if (result?.action === "decline") return result;
					answer ??= result;
				} catch (error) {
					this.reportFailure(ext, "mcp_elicitation", error);
				}
			}
		}
		return answer;
	}

	/** pi-rust: each handler sees the answer so far and can replace it. */
	async emitMcpElicitationResult(event) {
		const ctx = this.createContext();
		let current;
		for (const { ext, handlers } of snapshot(this.extensions, "mcp_elicitation_result")) {
			for (const handler of handlers) {
				try {
					const seen = current ? { ...event, action: current.action, content: current.content } : event;
					current = elicitationResult("mcp_elicitation_result", await handler(seen, ctx)) ?? current;
				} catch (error) {
					this.reportFailure(ext, "mcp_elicitation_result", error);
				}
			}
		}
		return current;
	}

	/** pi-rust `prompt_expansion`: the first block wins; `context` texts join in handler order. */
	async emitPromptExpansion(event) {
		const ctx = this.createContext();
		const contexts = [];
		for (const { ext, handlers } of snapshot(this.extensions, "prompt_expansion")) {
			for (const handler of handlers) {
				try {
					const result = await handler(event, ctx);
					if (result?.action === "block") return { action: "block", reason: result.reason };
					if (result?.action === "context" && typeof result.text === "string") contexts.push(result.text);
				} catch (error) {
					this.reportFailure(ext, "prompt_expansion", error);
				}
			}
		}
		return contexts.length > 0 ? { action: "context", text: contexts.join("\n\n") } : { action: "continue" };
	}

	async emitContext(messages) {
		const ctx = this.createContext();
		let current = messages;
		for (const { ext, handlers } of snapshot(this.extensions, "context")) {
			for (const handler of handlers) {
				try {
					const visible = current.filter((message) => message.role !== "system");
					const visibleSnapshot = visible.slice();
					const result = await handler({ type: "context", messages: visible }, ctx);
					const returned = result?.messages ?? (sameMessages(visible, visibleSnapshot) ? undefined : visible);
					if (!returned) continue;
					current = restoreSystemMessages(current, visibleSnapshot, returned);
				} catch (error) {
					this.reportFailure(ext, "context", error);
				}
			}
		}
		for (const { ext, handlers } of snapshot(this.extensions, "context_with_system")) {
			for (const handler of handlers) {
				try {
					const hadSystem = current[0]?.role === "system";
					const result = await handler({ type: "context_with_system", messages: current }, ctx);
					current = result?.messages ?? current;
					if (hadSystem && current[0]?.role !== "system") {
						this.emitError({
							extensionPath: ext.path,
							event: "context_with_system",
							error: "Handler removed the leading system message; the request has no prompt or initial tool declarations. Keep it at index 0 or replace a dropped prefix with getCurrentSystemMessage().",
						});
					}
				} catch (error) {
					this.reportFailure(ext, "context_with_system", error);
				}
			}
		}
		return current;
	}

	async emitBeforeProviderRequest(payload) {
		const ctx = this.createContext();
		let current = payload;
		for (const { ext, handlers } of snapshot(this.extensions, "before_provider_request")) {
			for (const handler of handlers) {
				try {
					const result = await handler({ type: "before_provider_request", payload: current }, ctx);
					if (result !== undefined) current = result;
				} catch (error) {
					this.reportFailure(ext, "before_provider_request", error);
				}
			}
		}
		return current;
	}

	/** pi-rust `before_provider_auth`: the first handler that names an account wins. */
	async emitBeforeProviderAuth(event) {
		const ctx = this.createContext();
		for (const { ext, handlers } of snapshot(this.extensions, "before_provider_auth")) {
			for (const handler of handlers) {
				try {
					const result = await handler({ type: "before_provider_auth", ...event }, ctx);
					if (typeof result?.authProvider === "string" && result.authProvider) return { authProvider: result.authProvider };
				} catch (error) {
					this.reportFailure(ext, "before_provider_auth", error);
				}
			}
		}
		return {};
	}

	async emitBeforeProviderHeaders(headers) {
		const ctx = this.createContext();
		for (const { ext, handlers } of snapshot(this.extensions, "before_provider_headers")) {
			for (const handler of handlers) {
				try {
					await handler({ type: "before_provider_headers", headers }, ctx);
				} catch (error) {
					this.reportFailure(ext, "before_provider_headers", error);
				}
			}
		}
		rememberHeaders(headers);
		return headers;
	}

	async emitBeforeAgentStart(trigger, options) {
		const render = () => call("prompt.build", options);
		const ctx = Object.defineProperties({}, Object.getOwnPropertyDescriptors(this.createContext()));
		ctx.getSystemPrompt = () => {
			this.assertActive();
			return render();
		};
		const messages = [];
		for (const { ext, handlers } of snapshot(this.extensions, "before_agent_start")) {
			for (const handler of handlers) {
				try {
					const event = {
						...trigger,
						type: "before_agent_start",
						get systemPrompt() {
							return render();
						},
						systemPromptOptions: options,
					};
					const result = await handler(event, ctx);
					if (result?.message) messages.push(result.message);
					if (result?.systemPrompt !== undefined) options.forceSystemPrompt = result.systemPrompt;
				} catch (error) {
					this.reportFailure(ext, "before_agent_start", error);
				}
			}
		}
		return { messages, systemPromptOptions: options };
	}

	async emitResourcesDiscover(cwd, reason) {
		const ctx = this.createContext();
		const found = { skillPaths: [], promptPaths: [], themePaths: [], contextEntries: [] };
		for (const { ext, handlers } of snapshot(this.extensions, "resources_discover")) {
			for (const handler of handlers) {
				try {
					const result = await handler({ type: "resources_discover", cwd, reason }, ctx);
					for (const key of ["skillPaths", "promptPaths", "themePaths"]) {
						for (const path of result?.[key] ?? []) found[key].push({ path, extensionPath: ext.path });
					}
					// pi-rust: deferrable context, such as agent configs.
					const entries = Array.isArray(result?.contextEntries) ? result.contextEntries : [];
					for (const entry of entries) {
						if (entry && typeof entry === "object") found.contextEntries.push({ ...entry, extensionPath: ext.path });
					}
				} catch (error) {
					this.reportFailure(ext, "resources_discover", error);
				}
			}
		}
		return found;
	}

	async emitInput(text, images, source, streamingBehavior) {
		const ctx = this.createContext();
		let currentText = text;
		let currentImages = images;
		for (const { ext, handlers } of snapshot(this.extensions, "input")) {
			for (const handler of handlers) {
				try {
					const event = { type: "input", text: currentText, images: currentImages, source, streamingBehavior };
					const result = await handler(event, ctx);
					if (result?.action === "handled") return result;
					if (result?.action === "transform") {
						currentText = result.text;
						currentImages = result.images ?? currentImages;
					}
				} catch (error) {
					this.reportFailure(ext, "input", error);
				}
			}
		}
		return currentText !== text || currentImages !== images
			? { action: "transform", text: currentText, images: currentImages }
			: { action: "continue" };
	}

	/**
	 * `turn_end` and `agent_before_settle` (upstream `emitBoundary`). pi-rust: a handler's
	 * `event.context` is built when the handler first reads it, and drafts are checked again only
	 * after they change, so handlers that do not read the context cost no pass over the session.
	 */
	async emitBoundary(baseEvent) {
		const ctx = this.createContext();
		const kind = baseEvent.type;
		let entries = [];
		let shouldContinue = false;
		let valid = true;
		// As upstream, the context shows the last drafts that applied.
		let contextDrafts = "[]";
		let checked = { drafts: "[]", error: undefined };
		for (const { ext, handlers } of snapshot(this.extensions, kind)) {
			for (const handler of handlers) {
				try {
					const event = { ...baseEvent, entries, continue: shouldContinue };
					defineBoundaryContext(event, kind, contextDrafts);
					const result = await handler(event, ctx);
					if (result?.entries !== undefined) entries = result.entries;
					if (result?.continue !== undefined) shouldContinue = result.continue;
				} catch (error) {
					this.reportFailure(ext, kind, error);
				}
				const drafts = draftsText(entries);
				if (drafts === undefined || drafts !== checked.drafts) checked = { drafts, error: checkDrafts(kind, entries) };
				valid = checked.error === undefined;
				if (valid) {
					contextDrafts = checked.drafts ?? contextDrafts;
				} else {
					this.emitError({
						extensionPath: ext.path,
						event: kind,
						error: `Invalid boundary entries: ${errorMessage(checked.error)}`,
						stack: errorStack(checked.error),
					});
				}
			}
		}
		return valid ? { entries, continue: shouldContinue, valid: true } : { entries: [], continue: false, valid: false };
	}

	/** The first handler deciding `yes` or `no` wins; `undecided` falls through. */
	async emitProjectTrust(event, ctx) {
		const errors = [];
		for (const { ext, handlers } of snapshot(this.extensions, "project_trust")) {
			for (const handler of handlers) {
				try {
					const result = await handler(event, ctx);
					if (!result || result.trusted === "undecided") continue;
					return { result, errors };
				} catch (error) {
					errors.push({ extensionPath: ext.path, event: event.type, error: errorMessage(error) });
				}
			}
		}
		return { errors };
	}
}

/** `event.context` of a boundary handler, built on its first read from `drafts` (JSON text). */
function defineBoundaryContext(event, kind, drafts) {
	let context;
	let built = false;
	Object.defineProperty(event, "context", {
		configurable: true,
		enumerable: true,
		get() {
			if (!built) {
				context = boundaryContext(call("boundary.plan", kind, JSON.parse(drafts)));
				built = true;
			}
			return context;
		},
		set(value) {
			context = value;
			built = true;
		},
	});
}

/** The drafts as JSON text, which also keeps them from later changes; `undefined` when they are not JSON. */
function draftsText(entries) {
	try {
		return JSON.stringify(entries);
	} catch {
		return undefined;
	}
}

/** The error of drafts that do not apply to the session, else `undefined`. */
function checkDrafts(kind, entries) {
	try {
		call("boundary.check", kind, entries);
		return undefined;
	} catch (error) {
		return error ?? new Error("Invalid boundary entries");
	}
}
