// Pure session-entry helpers of upstream `core/session-manager.ts` and the message builders of
// `core/messages.ts` they use.
import { randomUUID } from "node:crypto";

const CURRENT_SESSION_VERSION = 3;

export function createBranchSummaryMessage(summary, fromId, timestamp) {
	return { role: "branchSummary", summary, fromId, timestamp: new Date(timestamp).getTime() };
}

export function createCompactionSummaryMessage(summary, tokensBefore, timestamp) {
	return { role: "compactionSummary", summary, tokensBefore, timestamp: new Date(timestamp).getTime() };
}

export function createCustomMessage(customType, content, display, details, timestamp) {
	return { role: "custom", customType, content, display, details, timestamp: new Date(timestamp).getTime() };
}

function generateId(ids) {
	for (let i = 0; i < 100; i++) {
		const id = randomUUID().slice(0, 8);
		if (!ids.has(id)) return id;
	}
	return randomUUID();
}

function migrateV1ToV2(entries) {
	const ids = new Set();
	let prevId = null;
	for (const entry of entries) {
		if (entry.type === "session") {
			entry.version = 2;
			continue;
		}
		entry.id = generateId(ids);
		entry.parentId = prevId;
		prevId = entry.id;
		if (entry.type === "compaction" && typeof entry.firstKeptEntryIndex === "number") {
			const targetEntry = entries[entry.firstKeptEntryIndex];
			if (targetEntry && targetEntry.type !== "session") entry.firstKeptEntryId = targetEntry.id;
			delete entry.firstKeptEntryIndex;
		}
	}
}

function migrateV2ToV3(entries) {
	for (const entry of entries) {
		if (entry.type === "session") {
			entry.version = 3;
			continue;
		}
		if (entry.type === "message" && entry.message?.role === "hookMessage") entry.message.role = "custom";
	}
}

export function migrateSessionEntries(entries) {
	const header = entries.find((entry) => entry.type === "session");
	const version = header?.version ?? 1;
	if (version >= CURRENT_SESSION_VERSION) return;
	if (version < 2) migrateV1ToV2(entries);
	if (version < 3) migrateV2ToV3(entries);
}

export function parseSessionEntries(content) {
	const entries = [];
	for (const line of content.trim().split("\n")) {
		if (!line.trim()) continue;
		try {
			entries.push(JSON.parse(line));
		} catch {}
	}
	return entries;
}

export function getLatestCompactionEntry(entries) {
	for (let i = entries.length - 1; i >= 0; i--) {
		if (entries[i].type === "compaction") return entries[i];
	}
	return null;
}

function buildSessionPath(entries, leafId, byId) {
	const index = byId ?? new Map(entries.map((entry) => [entry.id, entry]));
	if (leafId === null) return [];
	const leaf = (leafId ? index.get(leafId) : undefined) ?? entries[entries.length - 1];
	if (!leaf) return [];
	const path = [];
	for (let current = leaf; current; current = current.parentId ? index.get(current.parentId) : undefined) {
		path.push(current);
	}
	return path.reverse();
}

function getSessionContextSettings(path) {
	let thinkingLevel = "off";
	let model = null;
	for (const entry of path) {
		if (entry.type === "thinking_level_change") {
			thinkingLevel = entry.thinkingLevel;
		} else if (entry.type === "model_change") {
			model = { provider: entry.provider, modelId: entry.modelId };
		} else if (entry.type === "message" && entry.message.role === "assistant") {
			model = { provider: entry.message.provider, modelId: entry.message.model };
		}
	}
	return { thinkingLevel, model };
}

export function sessionEntryToContextMessages(entry) {
	if (entry.type === "message") {
		const message = entry.message;
		if (message.role === "system" && message.content == null) return [{ ...message, content: "" }];
		if (["user", "assistant", "toolResult"].includes(message.role) && message.content == null) {
			return [{ ...message, content: [] }];
		}
		return [message];
	}
	if (entry.type === "custom_message") {
		return [createCustomMessage(entry.customType, entry.content ?? [], entry.display, entry.details, entry.timestamp)];
	}
	if (entry.type === "branch_summary" && entry.summary) {
		return [createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp)];
	}
	if (entry.type === "compaction") {
		const summary = createCompactionSummaryMessage(entry.summary, entry.tokensBefore, entry.timestamp);
		return entry.systemMessage ? [entry.systemMessage, summary] : [summary];
	}
	return [];
}

export function buildContextEntries(entries, leafId, byId) {
	const path = buildSessionPath(entries, leafId, byId);
	let compaction = null;
	for (const entry of path) {
		if (entry.type === "compaction") compaction = entry;
	}
	if (!compaction) return path;
	const compactionIdx = path.findIndex((entry) => entry.id === compaction.id);
	if (compactionIdx < 0) return path;

	const contextEntries = [compaction];
	let foundFirstKept = false;
	for (let i = 0; i < compactionIdx; i++) {
		const entry = path[i];
		if (entry.id === compaction.firstKeptEntryId) foundFirstKept = true;
		if (foundFirstKept && !(entry.type === "message" && entry.message.role === "system")) contextEntries.push(entry);
	}
	contextEntries.push(...path.slice(compactionIdx + 1));
	return contextEntries;
}

function projectContextEntry(entry, edit) {
	const messages = sessionEntryToContextMessages(entry);
	if (!edit) return messages;
	const replacement = edit.replacement;
	if (replacement === null) return [];
	return messages.map((message) => {
		if (!["user", "assistant", "toolResult", "custom"].includes(message.role)) return message;
		const content =
			(message.role === "assistant" || message.role === "toolResult") && typeof replacement.content === "string"
				? [{ type: "text", text: replacement.content }]
				: replacement.content;
		return { ...message, content };
	});
}

export function buildSessionProjection(entries, leafId, byId) {
	const path = buildSessionPath(entries, leafId, byId);
	const { thinkingLevel, model } = getSessionContextSettings(path);
	const contextEntries = buildContextEntries(entries, leafId, byId);
	const edits = new Map();
	for (const entry of contextEntries) {
		if (entry.type === "context_edit") edits.set(entry.targetId, entry);
	}
	// Only the newest compaction, at index zero, contributes a checkpoint and summary.
	const projectedEntries = contextEntries.map((sourceEntry, index) => ({
		sourceEntry,
		messages:
			sourceEntry.type === "compaction" && index > 0 ? [] : projectContextEntry(sourceEntry, edits.get(sourceEntry.id)),
	}));
	return {
		entries: projectedEntries,
		messages: projectedEntries.flatMap((entry) => entry.messages),
		thinkingLevel,
		model,
	};
}

export function buildSessionContext(entries, leafId, byId) {
	const { messages, thinkingLevel, model } = buildSessionProjection(entries, leafId, byId);
	return { messages, thinkingLevel, model };
}
