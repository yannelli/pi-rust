// Writes golden SQLite and JSONL fixtures with the upstream TypeScript storages. Development-time only.
import { mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
// Usage: PI_UPSTREAM=<upstream pi checkout> node generate.ts <out-dir>
const upstream = process.env.PI_UPSTREAM;
if (!upstream) throw new Error("set PI_UPSTREAM to an upstream pi checkout");
const { BACKGROUND_CONTEXT } = await import(join(upstream, "packages/chord/src/context/index.ts"));
const { NodeExecutionEnv } = await import(join(upstream, "packages/durable/src/env/node.ts"));
const { JsonlStorage } = await import(join(upstream, "packages/durable/src/storage/jsonl/index.ts"));
const { openNodeSqliteStorage } = await import(join(upstream, "packages/durable/src/storage/sqlite/node.ts"));

const out = process.argv[2]!;
const context = BACKGROUND_CONTEXT;
const session = { kind: "session" };
const root = { kind: "conversation", conversationId: 1 };
const child = { kind: "conversation", conversationId: 10 };
const task = (state: object, extra: object = {}) => ({
	id: 5, conversationId: 1, kind: "fixture.task", version: 2, input: { prompt: "héllo", n: 1.5 },
	background: false, abortRequested: false, state, ...extra,
});
const batches: object[][] = [
	[{ type: "conversation", value: { id: 1 } }],
	[
		{ type: "entry", value: { kind: "user", data: { text: "first ✓", list: [1, 2.25, null, true] }, id: 2, conversationId: 1 } },
		{ type: "entry", value: { kind: "marker", id: 3, conversationId: 1, head: 2 } },
		{ type: "task", value: task({ status: "pending", checkpoint: { phase: "start" } }) },
		{ type: "submission", value: { conversationId: 1, type: "input", requestId: "req-1", status: "queued", id: 6 } },
	],
	[
		{ type: "task", value: task({ status: "running", checkpoint: { phase: "effect" } }, { memos: { winner: "a" }, startedAt: 1700000000123 }) },
		{ type: "submission", value: { conversationId: 1, type: "input", requestId: "req-1", status: "placed", entry: 2, id: 6 } },
		{ type: "document.create", record: { id: 7, kind: "settings", scope: session }, content: { version: 1, kind: "base", value: { count: 0, name: "ünïcode" } } },
		{ type: "document.create", record: { id: 8, kind: "notes", key: "k\u0000ey", scope: root, history: "rewindable", fork: "asOf" }, content: { version: 1, kind: "base", value: { items: ["a"] } } },
		{ type: "document.create", record: { id: 9, kind: "live", scope: root, history: "latest", fork: "current" }, content: { version: 1, kind: "base", value: { tools: {} } } },
	],
	[
		{ type: "document.change", id: 7, content: { version: 1, kind: "delta", ops: [["s", ["count"], 1], ["a", ["name"], "!"]] } },
		{ type: "document.change", id: 8, content: { version: 1, kind: "delta", ops: [["p", ["items"], 1, 0, ["b", "c"]]] } },
		{ type: "document.change", id: 9, content: { version: 1, kind: "delta", ops: [["s", ["tools", "read"], { state: "running" }]] } },
	],
	[
		{ type: "conversation", value: { id: 10, parent: { conversationId: 1, at: 3 }, owner: { conversationId: 1, taskId: 5 } } },
		{ type: "document.copy", record: { id: 11, kind: "notes", key: "k\u0000ey", scope: child, history: "rewindable", fork: "asOf" }, source: { id: 8, at: 4 } },
		{ type: "document.copy", record: { id: 12, kind: "live", scope: child, history: "latest", fork: "current" }, source: { id: 9, at: "current" } },
		{ type: "entry", value: { kind: "assistant", data: { text: "child" }, id: 13, conversationId: 10, byTaskId: 5 } },
	],
	[
		{ type: "document.change", id: 7, content: { version: 2, kind: "base", value: { count: 2, migrated: true } } },
		{ type: "document.change", id: 8, content: { version: 1, kind: "base", value: { items: ["checkpoint"] } } },
		{ type: "document.change", id: 9, content: { version: 1, kind: "base", value: { tools: {} } } },
	],
	[
		{ type: "document.change", id: 8, content: { version: 1, kind: "delta", ops: [["r", { items: ["replaced"] }]] } },
		{ type: "document.retire", id: 9 },
		{ type: "task", value: task({ status: "terminal", outcome: { status: "completed", result: null } }, { startedAt: 1700000000123, endedAt: 1700000000456.5 }) },
		{ type: "submission", value: { conversationId: 1, type: "input", requestId: "req-1", status: "done", entry: 2, answer: 13, id: 6 } },
	],
	[
		{ type: "task", value: { id: 14, conversationId: 10, kind: "fixture.child", version: 1, input: null, owner: 5, background: true, abortRequested: true, abortReason: "restart", state: { status: "waiting", checkpoint: {}, on: [5], policy: "failFast" } } },
		{ type: "submission", value: { conversationId: 10, type: "write", status: "unanswered", reason: "closed", detail: { retry: false }, id: 15 } },
	],
];

async function reads(storage: any) {
	const expected: Record<string, unknown> = {};
	expected.conversations = [await storage.conversation(1, context), await storage.conversation(10, context)];
	expected.entries = await Promise.all([2, 3, 13].map((id) => storage.entry(id, context)));
	expected.visible = (await storage.scanEntries({ conversationId: 10 }, 100, undefined, context)).items;
	expected.tasks = [await storage.task(5, context), await storage.task(14, context)];
	expected.submissions = [await storage.submission(6, context), await storage.submission(15, context)];
	const documents = [];
	for (const id of [7, 8, 9, 11, 12]) {
		for (const at of ["current", 3, 4, 5, 6, 7, 8]) {
			let stored: unknown;
			try {
				stored = (await storage.document(id, at, context)) ?? null;
			} catch (error) {
				stored = { error: (error as Error).message };
			}
			documents.push({ id, at, stored });
		}
	}
	expected.documents = documents;
	return expected;
}

await rm(out, { recursive: true, force: true });
await mkdir(join(out, "jsonl"), { recursive: true });
const sqlite = await openNodeSqliteStorage(join(out, "storage.sqlite"));
const jsonl = await JsonlStorage.open(join(out, "jsonl"), new NodeExecutionEnv({ cwd: out }), context);
for (const batch of batches) {
	await sqlite.commit(batch as never, context);
	await jsonl.commit(batch as never, context);
}
const expectedSqlite = await reads(sqlite);
const expectedJsonl = await reads(jsonl);
if (JSON.stringify(expectedSqlite) !== JSON.stringify(expectedJsonl)) throw new Error("backends disagree");
await sqlite.close(context);
await jsonl.close(context);
await writeFile(join(out, "commits.json"), `${JSON.stringify(batches, null, 1)}\n`);
await writeFile(join(out, "expected.json"), `${JSON.stringify(expectedSqlite, null, 1)}\n`);
