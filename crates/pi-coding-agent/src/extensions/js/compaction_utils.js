// Pure helpers of upstream `core/compaction/` (cut points, usage, branch preparation). Token
// estimates come from Rust, as `estimateTokens` does.
import { call } from "./bridge.js";
import {
	createBranchSummaryMessage,
	createCompactionSummaryMessage,
	createCustomMessage,
	sessionEntryToContextMessages,
} from "./session_utils.js";

const estimateTokens = (message) => call("messages.estimateTokens", message);

function getAssistantUsage(message) {
	if (message.role !== "assistant" || !("usage" in message)) return undefined;
	if (message.stopReason === "aborted" || message.stopReason === "error" || !message.usage) return undefined;
	const usage = message.usage;
	const tokens = usage.totalTokens || usage.input + usage.output + usage.cacheRead + usage.cacheWrite;
	return tokens > 0 ? usage : undefined;
}

export function getLastAssistantUsage(entries) {
	for (let i = entries.length - 1; i >= 0; i--) {
		const entry = entries[i];
		if (entry.type !== "message") continue;
		const usage = getAssistantUsage(entry.message);
		if (usage) return usage;
	}
	return undefined;
}

const CUT_POINT_ROLES = ["user", "assistant", "bashExecution", "custom", "branchSummary", "compactionSummary"];
const TURN_START_ROLES = ["user", "bashExecution", "custom", "branchSummary", "compactionSummary"];

function isTurnStartEntry(entry) {
	if (entry.type === "compaction") return false;
	return sessionEntryToContextMessages(entry).some((message) => TURN_START_ROLES.includes(message.role));
}

function findValidCutPoints(entries, startIndex, endIndex) {
	const cutPoints = [];
	for (let i = startIndex; i < endIndex; i++) {
		const entry = entries[i];
		if (entry.type === "compaction") continue;
		if (sessionEntryToContextMessages(entry).some((message) => CUT_POINT_ROLES.includes(message.role))) {
			cutPoints.push(i);
		}
	}
	return cutPoints;
}

export function findTurnStartIndex(entries, entryIndex, startIndex) {
	for (let i = entryIndex; i >= startIndex; i--) {
		if (isTurnStartEntry(entries[i])) return i;
	}
	return -1;
}

export function findCutPoint(entries, startIndex, endIndex, keepRecentTokens) {
	const cutPoints = findValidCutPoints(entries, startIndex, endIndex);
	if (cutPoints.length === 0) return { firstKeptEntryIndex: startIndex, turnStartIndex: -1, isSplitTurn: false };

	let accumulatedTokens = 0;
	let cutIndex = cutPoints[0];
	for (let i = endIndex - 1; i >= startIndex; i--) {
		const messages = sessionEntryToContextMessages(entries[i]);
		const messageTokens = messages.reduce((sum, message) => sum + estimateTokens(message), 0);
		if (messageTokens === 0) continue;
		accumulatedTokens += messageTokens;
		if (accumulatedTokens >= keepRecentTokens) {
			cutIndex = cutPoints.find((candidate) => candidate >= i) ?? cutPoints[cutPoints.length - 1];
			break;
		}
	}

	while (cutIndex > startIndex) {
		const prevEntry = entries[cutIndex - 1];
		if (prevEntry.type === "compaction" || sessionEntryToContextMessages(prevEntry).length > 0) break;
		cutIndex--;
	}

	const startsTurn = isTurnStartEntry(entries[cutIndex]);
	const turnStartIndex = startsTurn ? -1 : findTurnStartIndex(entries, cutIndex, startIndex);
	return { firstKeptEntryIndex: cutIndex, turnStartIndex, isSplitTurn: !startsTurn && turnStartIndex !== -1 };
}

export function collectEntriesForBranchSummary(session, oldLeafId, targetId) {
	if (!oldLeafId) return { entries: [], commonAncestorId: null };
	const oldPath = new Set(session.getBranch(oldLeafId).map((entry) => entry.id));
	const targetPath = session.getBranch(targetId);
	let commonAncestorId = null;
	for (let i = targetPath.length - 1; i >= 0; i--) {
		if (oldPath.has(targetPath[i].id)) {
			commonAncestorId = targetPath[i].id;
			break;
		}
	}
	const entries = [];
	let current = oldLeafId;
	while (current && current !== commonAncestorId) {
		const entry = session.getEntry(current);
		if (!entry) break;
		entries.push(entry);
		current = entry.parentId;
	}
	entries.reverse();
	return { entries, commonAncestorId };
}

function getMessageFromEntry(entry) {
	switch (entry.type) {
		case "message":
			return entry.message.role === "toolResult" ? undefined : entry.message;
		case "custom_message":
			return createCustomMessage(entry.customType, entry.content, entry.display, entry.details, entry.timestamp);
		case "branch_summary":
			return createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp);
		case "compaction":
			return createCompactionSummaryMessage(entry.summary, entry.tokensBefore, entry.timestamp);
	}
	return undefined;
}

function addFileOp(toolName, args, fileOps) {
	const path = typeof args?.path === "string" ? args.path : undefined;
	if (!path) return;
	if (toolName === "read") fileOps.read.add(path);
	else if (toolName === "write") fileOps.written.add(path);
	else if (toolName === "edit") fileOps.edited.add(path);
}

function extractFileOpsFromMessage(message, fileOps) {
	if (message.role === "toolResult") {
		for (const nested of message.nestedCalls?.calls ?? []) addFileOp(nested.name, nested.arguments, fileOps);
		return;
	}
	if (message.role !== "assistant" || !Array.isArray(message.content)) return;
	for (const block of message.content) {
		if (block?.type === "toolCall" && "arguments" in block && "name" in block) {
			addFileOp(block.name, block.arguments, fileOps);
		}
	}
}

export function prepareBranchEntries(entries, tokenBudget = 0) {
	const messages = [];
	const fileOps = { read: new Set(), written: new Set(), edited: new Set() };
	let totalTokens = 0;

	for (const entry of entries) {
		if (entry.type !== "branch_summary" || entry.fromHook || !entry.details) continue;
		if (Array.isArray(entry.details.readFiles)) for (const file of entry.details.readFiles) fileOps.read.add(file);
		if (Array.isArray(entry.details.modifiedFiles)) {
			for (const file of entry.details.modifiedFiles) fileOps.edited.add(file);
		}
	}

	for (let i = entries.length - 1; i >= 0; i--) {
		const entry = entries[i];
		const message = getMessageFromEntry(entry);
		if (!message) continue;
		extractFileOpsFromMessage(message, fileOps);
		const tokens = estimateTokens(message);
		if (tokenBudget > 0 && totalTokens + tokens > tokenBudget) {
			const isSummary = entry.type === "compaction" || entry.type === "branch_summary";
			if (isSummary && totalTokens < tokenBudget * 0.9) {
				messages.unshift(message);
				totalTokens += tokens;
			}
			break;
		}
		messages.unshift(message);
		totalTokens += tokens;
	}
	return { messages, fileOps, totalTokens };
}
