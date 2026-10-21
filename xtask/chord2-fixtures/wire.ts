// Wire fixtures for chord2: every frame type as each transport writes it, payload packing around the threshold,
// frame validation verdicts and wire errors. Called by generate.ts.

import { join } from "node:path";
import { pathToFileURL } from "node:url";

interface Options {
	readonly upstream: string;
	readonly write: (name: string, value: unknown) => void;
	readonly plain: (value: unknown) => unknown;
	readonly chatV1: any;
	readonly index: any;
}

export async function writeWireFixtures({ upstream, write, plain, chatV1, index }: Options): Promise<void> {
	const load = (path: string) => import(pathToFileURL(join(upstream, "packages/chord/src", path)).href);
	const link = await load("link.ts");
	const wire = await load("wire.ts");
	const errors = await load("errors.ts");
	const { hash64 } = await load("schema.ts");
	/** Long texts as their UTF-16 length and hash64, so the fixture stays small. */
	const digest = (text: string) => (text.length <= 20_000 ? text : { length: text.length, hash: hash64(text) });

	const errorOf = (code: string, message: string, reason?: string) =>
		wire.toWireError(new errors.ChordError(code, message, reason === undefined ? undefined : { reason }));
	const unicode = {
		text: "\u00E9t\u00E9 \u{1F600} \u4E2D",
		ctrl: "a\u0001b\n\"\\",
		sep: "\u2028",
		n: [1e21, 1.5, -0, 5e-324],
	};
	const root = { rootId: "r1", codeVersion: 2 };
	const frames: Record<string, unknown> = {
		hello: { t: "hello", protocol: wire.PROTOCOL_VERSION, name: "server", kind: "host", codeVersion: 1, acks: true },
		helloClient: { t: "hello", name: "browser", kind: "client", codeVersion: 0 },
		ack: { t: "ack", n: 3 },
		hold: {
			t: "hold",
			id: 1,
			at: "session:S1",
			token: chatV1.description,
			input: { session: "S1" },
			principal: { worker: "session:S1" },
			origin: "c1",
			state: false,
			again: true,
		},
		holdLocal: { t: "hold", id: 2, at: "server", token: index.description, input: {} },
		release: { t: "release", id: 1 },
		shape: {
			t: "shape",
			id: 1,
			variant: "participant",
			members: ["view", "send"],
			contract: chatV1.fingerprint,
			state: ["view"],
		},
		shapeLocal: { t: "shape", id: 2, members: [], contract: index.fingerprint, state: [] },
		gone: { t: "gone", id: 1, error: errorOf("gone", "object disposed") },
		goneReason: { t: "gone", id: 1, error: errorOf("unavailable", "no route", "unknown_type") },
		call: { t: "call", id: 7, hold: 1, member: "send", args: { text: "hi" }, root },
		callNoRoot: { t: "call", id: 8, hold: 1, member: "send", args: unicode },
		callNoArgs: { t: "call", id: 9, hold: 1, member: "stop", root },
		callNull: { t: "call", id: 10, hold: 1, member: "send", args: null },
		result: { t: "result", id: 7, value: { id: 1 } },
		resultVoid: { t: "result", id: 9 },
		resultError: { t: "result", id: 8, error: errorOf("invalid", "args: / must be object", "args") },
		cancel: { t: "cancel", id: 7 },
		sub: { t: "sub", hold: 1 },
		unsub: { t: "unsub", hold: 1 },
		resync: { t: "resync", hold: 1, m: "view" },
		stateSnap: { t: "state", hold: 1, m: "view", as: "snap", value: { text: "hello" } },
		stateSnapUndefined: { t: "state", hold: 1, m: "view", as: "snap" },
		stateReset: { t: "state", hold: 1, m: "view", as: "reset", value: unicode },
		stateOps: { t: "state", hold: 1, m: "view", as: "ops", ops: [[0, ["text"], "x"], [1, []]] },
		stateError: { t: "state", hold: 1, m: "view", as: "error", error: errorOf("invalid", "state view: bad", "state") },
		ping: { t: "ping", n: 1 },
		pong: { t: "pong", n: -3 },
		bye: { t: "bye", reason: "protocol-version 4" },
		byeBare: { t: "bye" },
		rootEnd: { t: "root_end", rootId: "r1" },
		invalidate: { t: "invalidate", pattern: { user: "u1", nested: { a: [1] } } },
		prepare: {
			t: "prepare",
			reload: 1,
			codeVersion: 3,
			plugins: ["a", "b"],
			options: { remove: ["c"], gateDeadline: 5000, retireDeadline: 1.5 },
		},
		prepareSync: { t: "prepare", reload: 2, codeVersion: 3, plugins: [], options: { remove: [] } },
		voteError: { t: "vote", reload: 1, error: errorOf("aborted", "gate deadline", "gate_deadline") },
		voteReport: {
			t: "vote",
			reload: 1,
			report: { codeVersion: 3, previous: 2, changed: ["t.a"], removed: [], prepared: ["t.a\u0000{}\u0000host"], gated: [] },
		},
		commit: { t: "commit", reload: 1 },
		retire: { t: "retire", codeVersion: 2 },
		abort: { t: "abort", reload: 1 },
	};

	/** What each transport writes for a frame: the IPC/memory object form and the WebSocket message. */
	function transports(frame: unknown) {
		const sent: unknown[] = [];
		const channel = { connected: true, send: (message: unknown) => sent.push(message), on: () => {}, off: () => {} };
		link.ipcLink(channel).send(frame);
		const socket = (into: unknown[]) => ({
			readyState: 1,
			send: (data: unknown) => into.push(data),
			close: () => {},
			addEventListener: () => {},
		});
		const wsOut: unknown[] = [];
		const wsPlainOut: unknown[] = [];
		const wsSize = link.webSocketLink(socket(wsOut), { compress: true }).send(frame);
		link.webSocketLink(socket(wsPlainOut), { compress: false }).send(frame);
		const message = (data: unknown) =>
			typeof data === "string"
				? { text: digest(data) }
				: {
						binary: Buffer.from(data as Uint8Array).toString("base64"),
						inflated: digest(link.inflateMessage(data)),
					};
		return {
			object: digest(JSON.stringify((sent[0] as { chord2: unknown }).chord2)),
			ws: message(wsOut[0]),
			wsPlain: message(wsPlainOut[0]),
			wsSize,
			valid: wire.isFrame(frame),
		};
	}
	write("frames.json", {
		protocol: wire.PROTOCOL_VERSION,
		packThreshold: link.PACK_THRESHOLD,
		frames: Object.entries(frames).map(([label, frame]) => ({ label, ...transports(frame) })),
	});

	// Payloads around the threshold, measured in UTF-16 code units of their JSON.
	const sized = (units: number) => "x".repeat(units - 2);
	const large = Array.from({ length: 2000 }, (_, item) => ({ item, text: `line ${item} \u00E9\u{1F600}` }));
	const packedFrames: Record<string, unknown> = {
		atThreshold: { t: "result", id: 1, value: sized(link.PACK_THRESHOLD) },
		overThreshold: { t: "result", id: 2, value: sized(link.PACK_THRESHOLD + 1) },
		multibyteUnder: { t: "result", id: 3, value: "\u00E9".repeat(40_000) },
		astralOver: { t: "state", hold: 4, m: "notes", as: "snap", value: "\u{1F600}".repeat(33_000) },
		largeArgs: { t: "call", id: 5, hold: 1, member: "echo", args: large, root: { rootId: "r", codeVersion: 1 } },
		largeReset: { t: "state", hold: 4, m: "notes", as: "reset", value: large },
	};
	write("packed.json", {
		frames: Object.entries(packedFrames).map(([label, frame]) => {
			const { args, value } = frame as { args?: unknown; value?: unknown };
			return { label, payload: digest(JSON.stringify(args ?? value)), ...transports(frame) };
		}),
	});

	const hold = (token: unknown, extra: Record<string, unknown> = {}) => ({
		t: "hold",
		id: 1,
		at: "server",
		token,
		input: {},
		...extra,
	});
	const token = (patch: Record<string, unknown>, base = index.description) => ({ ...base, ...patch });
	const member = (value: unknown) => token({ variants: { "": { m: value } } });
	const candidates: unknown[] = [
		...Object.values(frames),
		{ t: "hello", name: "x", kind: "host", codeVersion: 1, extra: { anything: true } },
		{ t: "hello", name: "", kind: "host", codeVersion: 1 },
		{ t: "hello", name: "x", kind: "browser", codeVersion: 1 },
		{ t: "hello", name: "x", kind: "host", codeVersion: -1 },
		{ t: "hello", name: "x", kind: "host", codeVersion: 1.5 },
		{ t: "hello", name: "x", kind: "host", codeVersion: 1, protocol: 0 },
		{ t: "hello", name: "x", kind: "host", codeVersion: 1, acks: "yes" },
		{ t: "ack", n: 0 },
		{ t: "ack", n: 2.0 },
		{ t: "hold", id: 1, at: "server", token: index.description },
		hold(index.description, { input: null }),
		hold(index.description, { state: true }),
		hold(index.description, { again: false }),
		hold(index.description, { origin: "" }),
		hold(index.description, { id: 0 }),
		hold(token({ scope: "global" })),
		hold(token({ name: "" })),
		hold(token({ version: 1.5 })),
		hold(token({ input: [] })),
		hold(token({ host: { name: "" } }, chatV1.description)),
		hold(token({ host: { name: "a", prefix: "b" } }, chatV1.description)),
		hold(token({ extra: 1 }, chatV1.description)),
		hold(member({ kind: "method", args: true })),
		hold(member({ kind: "event" })),
		hold(member({ kind: "opaque" })),
		hold(token({ variants: { "": [] } })),
		{ t: "shape", id: 1, members: ["a"], contract: "x", state: [""] },
		{ t: "shape", id: 1, variant: "", members: [], contract: "", state: [] },
		{ t: "shape", id: 1, members: [1], contract: "x", state: [] },
		{ t: "gone", id: 1, error: { code: "nope", message: "x" } },
		{ t: "gone", id: 1, error: { code: "gone" } },
		{ t: "call", id: 1, hold: 1, member: "" },
		{ t: "call", id: 1, hold: 1, member: "m", root: { rootId: "r", codeVersion: 0 } },
		{ t: "result", id: 1, error: { code: "aborted", message: "m", reason: 1 } },
		{ t: "state", hold: 1, m: "x", as: "patch" },
		{ t: "state", hold: 1, m: "x", as: "ops", ops: [1] },
		{ t: "state", hold: 1, m: "x", as: "ops", ops: [[]] },
		{ t: "ping", n: -5 },
		{ t: "ping", n: 1.5 },
		{ t: "bye", reason: 3 },
		{ t: "root_end", rootId: "" },
		{ t: "invalidate", pattern: [] },
		{ t: "prepare", reload: 1, codeVersion: 1, plugins: [], options: {} },
		{ t: "prepare", reload: 1, codeVersion: 1, plugins: [] },
		{ t: "prepare", reload: 1, codeVersion: 1, plugins: [], options: { gateDeadline: "5" } },
		{ t: "vote", reload: 1, report: { codeVersion: 1, previous: 0, changed: [], removed: [], prepared: [] } },
		{ t: "vote", reload: 0 },
		{ t: "retire", codeVersion: 0 },
		{ t: "unknown" },
		{ t: 5 },
		{},
		[],
		null,
	];
	write(
		"is_frame.json",
		candidates.map((value) => ({ value: plain(value), valid: wire.isFrame(value) })),
	);

	const thrown = [
		new errors.ChordError("gone", "object disposed"),
		new errors.ChordError("invalid", "t.chat input: /session must be string", { reason: "input" }),
		new errors.ChordError("unavailable", "reload aborted: x", { reason: "reload_running" }),
		new errors.ChordError("aborted", "superseded: nested prefix"),
		new errors.ChordError("contract_mismatch", ""),
		new errors.ChordError("forbidden", "Forbidden: Capitalized"),
		new Error("plain error"),
		new TypeError("type error"),
	];
	write(
		"wire_errors.json",
		thrown.map((error) => {
			const sent = wire.toWireError(error);
			const back = wire.fromWireError(sent);
			return {
				sent,
				back: { code: back.code, reason: back.reason ?? null, message: back.message, retryable: back.retryable },
			};
		}),
	);
}
