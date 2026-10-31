// Drives the compaction exports of `@earendil-works/pi-coding-agent` with a scripted stream function
// and a faux provider, on upstream under Node (expected output) and in the extension host.
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** Request times and generated session ids differ per run; everything else must match. */
function stable(value) {
	if (Array.isArray(value)) return value.map(stable);
	if (value && typeof value === "object") {
		return Object.fromEntries(
			Object.entries(value).map(([key, entry]) => [key, key === "timestamp" ? "<time>" : stable(entry)]),
		);
	}
	return typeof value === "string" && UUID.test(value) ? "<uuid>" : value;
}

const usage = (input, output) => ({
	input,
	output,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: input + output,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});
const reply = (text, extra = {}) => ({
	role: "assistant",
	content: [{ type: "text", text }],
	api: "faux",
	provider: "faux",
	model: "m",
	usage: usage(10, 5),
	stopReason: "stop",
	timestamp: 0,
	...extra,
});
const model = (id, reasoning) => ({
	id,
	name: id,
	api: "faux",
	provider: "faux",
	baseUrl: "http://127.0.0.1:9",
	reasoning,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200000,
	maxTokens: 8192,
});

const MESSAGES = [
	{ role: "user", content: "Fix the parser in a.txt", timestamp: 1 },
	{
		role: "assistant",
		content: [
			{ type: "text", text: "Reading it." },
			{ type: "toolCall", id: "c1", name: "read", arguments: { path: "a.txt" } },
		],
		api: "faux",
		provider: "faux",
		model: "m",
		usage: usage(4, 3),
		stopReason: "toolUse",
		timestamp: 2,
	},
	{ role: "toolResult", toolCallId: "c1", toolName: "read", content: [{ type: "text", text: "parser" }], isError: false, timestamp: 3 },
];

/** A stream function that records each request and answers from `queue`. */
function scripted(queue, calls) {
	return async (_model, context, options) => {
		const { signal, ...rest } = options ?? {};
		calls.push({ context, options: rest, aborted: signal?.aborted ?? null });
		const next = queue.shift();
		return { result: async () => next };
	};
}

async function attempt(run) {
	try {
		return { value: await run() };
	} catch (failure) {
		return { error: failure instanceof Error ? failure.message : String(failure) };
	}
}

function entry(id, parentId, fields) {
	return { id, parentId, timestamp: "2026-01-01T00:00:00.000Z", ...fields };
}

const BRANCH = [
	entry("e1", null, { type: "compaction", summary: "Earlier work", firstKeptEntryId: "e2", tokensBefore: 900 }),
	entry("e2", "e1", { type: "message", message: MESSAGES[0] }),
	entry("e3", "e2", { type: "message", message: MESSAGES[1] }),
	entry("e4", "e3", { type: "message", message: MESSAGES[2] }),
	entry("e5", "e4", {
		type: "branch_summary",
		fromId: "e2",
		summary: "A side branch",
		details: { readFiles: ["side.txt"], modifiedFiles: ["out.txt"] },
	}),
	entry("e6", "e5", { type: "custom_message", customType: "note", content: "A note", display: true }),
	entry("e7", "e6", { type: "context_edit", targetId: "e3", replacement: null }),
];

export async function scenario(c) {
	const out = {};
	const run = async (name, queue, call) => {
		const calls = [];
		const result = await attempt(() => call(scripted(queue, calls)));
		out[name] = { ...result, calls };
	};
	const plain = model("plain", false);
	const thinking = model("thinking", true);

	await run("withUsage", [reply("Summary one")], (fn) =>
		c.generateSummaryWithUsage(MESSAGES, plain, undefined, "key-1", { "x-h": "1" }, undefined, undefined, undefined, undefined, fn));
	await run("update", [reply("Summary two")], (fn) =>
		c.generateSummary(MESSAGES, thinking, undefined, "key-2", undefined, undefined, "Focus on files", "Old summary", "high", fn, { E: "1" }, undefined, undefined, "session-1"));
	await run("noReasoning", [reply("Summary three")], (fn) =>
		c.generateSummary(MESSAGES, plain, undefined, "key-3", undefined, undefined, undefined, undefined, "high", fn));
	await run("error", [reply("", { stopReason: "error", errorMessage: "boom" })], (fn) =>
		c.generateSummary(MESSAGES, plain, undefined, "k", undefined, undefined, undefined, undefined, undefined, fn));
	await run("length", [reply("partial", { stopReason: "length" })], (fn) =>
		c.generateSummary(MESSAGES, plain, undefined, "k", undefined, undefined, undefined, undefined, undefined, fn));
	await run("toolCall", [{ ...reply("x"), content: [{ type: "toolCall", id: "t", name: "read", arguments: {} }] }], (fn) =>
		c.generateSummary(MESSAGES, plain, undefined, "k", undefined, undefined, undefined, undefined, undefined, fn));

	const retryEvents = [];
	const callbacks = {
		onRetryScheduled: (...args) => retryEvents.push(["scheduled", ...args]),
		onRetryAttemptStart: () => retryEvents.push(["start"]),
		onRetryFinished: (...args) => retryEvents.push(["finished", ...args]),
	};
	const transient = reply("", { stopReason: "error", errorMessage: "overloaded_error" });
	await run("retry", [transient, reply("Recovered")], (fn) =>
		c.generateSummary(MESSAGES, plain, undefined, "k", undefined, undefined, undefined, undefined, undefined, fn, undefined, { enabled: true, maxRetries: 1, baseDelayMs: 1 }, callbacks));
	out.retry.events = retryEvents;

	const controller = new AbortController();
	controller.abort();
	await run("abort", [reply("", { stopReason: "aborted" })], (fn) =>
		c.generateSummary(MESSAGES, plain, undefined, "k", undefined, controller.signal, undefined, undefined, undefined, fn));

	const fileOps = () => ({ read: new Set(["a.txt"]), written: new Set(["b.txt"]), edited: new Set(["c.txt"]) });
	const preparation = (split) => ({
		firstKeptEntryId: "k1",
		messagesToSummarize: MESSAGES.slice(0, 2),
		turnPrefixMessages: split ? [MESSAGES[2]] : [],
		isSplitTurn: split,
		tokensBefore: 1234,
		previousSummary: "Before",
		fileOps: fileOps(),
		settings: { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 },
	});
	await run("compact", [reply("Compacted")], (fn) => c.compact(preparation(false), plain, "k", undefined, "Keep tests", undefined, undefined, fn));
	await run("compactSplit", [reply("History", { usage: usage(10, 5) }), reply("Prefix", { usage: usage(7, 2) })], (fn) =>
		c.compact(preparation(true), plain, "k", undefined, undefined, undefined, undefined, fn));
	await run("compactNoKeptEntry", [reply("Compacted")], (fn) =>
		c.compact({ ...preparation(false), firstKeptEntryId: "" }, plain, "k", undefined, undefined, undefined, undefined, fn));

	const branch = (options) => (fn) => c.generateBranchSummary(BRANCH, { model: plain, apiKey: "k", signal: new AbortController().signal, streamFn: fn, ...options });
	await run("branch", [reply("Branch summary")], branch({}));
	await run("branchReplace", [reply("Replaced")], branch({ customInstructions: "Only list files", replaceInstructions: true }));
	await run("branchEmpty", [], (fn) => c.generateBranchSummary([], { model: plain, signal: new AbortController().signal, streamFn: fn }));
	await run("branchAborted", [reply("", { stopReason: "aborted" })], branch({}));

	const seen = [];
	const faux = c.registerFauxProvider({ api: "faux-compaction", provider: "faux-compaction", models: [{ id: "fm", name: "fm" }] });
	faux.setResponses([(context, options) => (seen.push({ context, apiKey: options?.apiKey ?? null }), c.fauxAssistantMessage("Faux summary"))]);
	out.defaultPath = { ...(await attempt(() => c.generateSummary(MESSAGES, faux.getModel(), undefined, "faux-key"))), seen };
	faux.unregister();

	return stable(out);
}
