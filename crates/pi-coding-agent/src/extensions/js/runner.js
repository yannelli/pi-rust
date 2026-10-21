// Event dispatch (upstream `core/extensions/runner.ts`): handlers run in load order, failures are
// reported as extension errors, and each event combines handler results as upstream does.
import { getCurrentSystemMessage } from "../pi-ai/src/utils/transcript.ts";
import { call, callAsync, errorMessage, errorStack } from "./bridge.js";
import { createCommandContext, createContext, createToolContext } from "./context.js";

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

export class Runner {
	extensions = [];
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

	createContext() {
		return createContext(this);
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
		const ctx = this.createContext();
		let result;
		for (const { ext, handlers } of snapshot(this.extensions, event.type)) {
			for (const handler of handlers) {
				try {
					const handlerResult = await handler(event, ctx);
					if (SESSION_BEFORE.has(event.type) && handlerResult) {
						result = handlerResult;
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

	async emitMessageEnd(event) {
		const ctx = this.createContext();
		let current = event.message;
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
		return needsContent && current.content == null ? { ...current, content: [] } : current;
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
		for (const { handlers } of snapshot(this.extensions, "tool_call")) {
			for (const handler of handlers) {
				const handlerResult = await handler(event, ctx);
				if (handlerResult) {
					result = handlerResult;
					if (result.block) return { block: true, reason: result.reason, input: event.input };
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
		const found = { skillPaths: [], promptPaths: [], themePaths: [] };
		for (const { ext, handlers } of snapshot(this.extensions, "resources_discover")) {
			for (const handler of handlers) {
				try {
					const result = await handler({ type: "resources_discover", cwd, reason }, ctx);
					for (const key of ["skillPaths", "promptPaths", "themePaths"]) {
						for (const path of result?.[key] ?? []) found[key].push({ path, extensionPath: ext.path });
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

	/** `turn_end` and `agent_before_settle`: Rust previews the context for each set of drafts. */
	async emitBoundary(baseEvent) {
		const ctx = this.createContext();
		const buildContext = (drafts) => callAsync("boundary.context", baseEvent.type, drafts);
		let entries = [];
		let shouldContinue = false;
		let context = await buildContext(entries);
		let valid = true;
		for (const { ext, handlers } of snapshot(this.extensions, baseEvent.type)) {
			for (const handler of handlers) {
				try {
					const result = await handler({ ...baseEvent, entries, continue: shouldContinue, context }, ctx);
					if (result?.entries !== undefined) entries = result.entries;
					if (result?.continue !== undefined) shouldContinue = result.continue;
				} catch (error) {
					this.reportFailure(ext, baseEvent.type, error);
				}
				try {
					context = await buildContext(entries);
					valid = true;
				} catch (error) {
					valid = false;
					this.emitError({
						extensionPath: ext.path,
						event: baseEvent.type,
						error: `Invalid boundary entries: ${errorMessage(error)}`,
						stack: errorStack(error),
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
