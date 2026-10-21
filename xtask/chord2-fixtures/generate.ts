// Golden fixtures for pi-chord's chord2 foundation (host2): hash64, canonical JSON, contract descriptions and
// fingerprints, schema checks (here) and wire frames (wire.ts). Run against an upstream checkout with node_modules:
//   node --experimental-strip-types xtask/chord2-fixtures/generate.ts <upstream-dir> crates/pi-chord/tests/fixtures/chord2

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { writeWireFixtures } from "./wire.ts";

const [upstream, out] = process.argv.slice(2);
if (upstream === undefined || out === undefined) {
	throw new Error("usage: generate.ts <upstream-dir> <out-dir>");
}
const load = (path: string) => import(pathToFileURL(join(upstream, path)).href);
const { Type } = await load("node_modules/typebox/build/index.mjs");
const src = "packages/chord/src";
const chord = await load(`${src}/contract.ts`);
const schema = await load(`${src}/schema.ts`);

mkdirSync(out, { recursive: true });
const write = (name: string, value: unknown) =>
	writeFileSync(join(out, name), `${JSON.stringify(value, null, "\t")}\n`);
/** A schema as plain JSON (TypeBox's `~kind` markers are not enumerable). */
const plain = (value: unknown) => JSON.parse(JSON.stringify(value));

// ─── hash64 and canonical JSON ───────────────────────────────────────────────

const texts = [
	"",
	"a",
	"hello world",
	"\u00E9t\u00E9",
	"\u4E2D\u6587",
	"\u{1F600} smile",
	"\uFFFF\u{10000}",
	'{"name":"t.chat","scope":"remote"}',
	"x".repeat(1000),
];
const canonicalValues = [
	null,
	true,
	0,
	-0,
	1.5,
	1e21,
	1e-7,
	0.1 + 0.2,
	123456789012345680000,
	"quote \" backslash \\ newline \n tab \t ctrl \u0001 sep \u2028",
	[3, 1, 2],
	{ b: 1, a: 2, c: { z: [], y: {} } },
	{ "10": 1, "2": 2, b: 3, a: 4, "": 5 },
	{ "\u{1F600}": 1, "\uFFFF": 2, "\u00E9": 3, e: 4, E: 5 },
	{ nested: [{ y: 1, x: 2 }, [{ b: null, a: false }]] },
];
write("hash.json", {
	hash64: texts.map((text) => ({ text, hash: schema.hash64(text) })),
	canonical: canonicalValues.map((value) => ({ value, canonical: schema.canonicalJson(value) })),
});

// ─── contracts ───────────────────────────────────────────────────────────────

const View = Type.Object({ text: Type.String() });
const chatV1 = chord.remote("t.chat", {
	version: 1,
	input: Type.Object({ session: Type.String(), conversation: Type.Number({ default: 0 }) }),
	host: chord.hostFamily("session:", "session"),
	variants: {
		viewer: { state: { view: View } },
		participant: {
			state: { view: View },
			methods: { send: chord.method(Type.Object({ text: Type.String() }), Type.Object({ id: Type.Number() })) },
		},
	},
});
const participant = (view: unknown, send: unknown) => ({
	...chatV1.def,
	variants: { ...chatV1.def.variants, participant: { state: { view }, methods: { send } } },
});
const additive = chord.remote(
	"t.chat",
	participant(
		Type.Object({ text: Type.String(), extra: Type.Number() }),
		chord.method(
			Type.Object({ text: Type.String(), urgent: Type.Optional(Type.Boolean()) }),
			Type.Object({ id: Type.Number(), at: Type.Number() }),
		),
	),
);
const required = chord.remote(
	"t.chat",
	participant(
		View,
		chord.method(Type.Object({ text: Type.String(), urgent: Type.Boolean() }), Type.Object({ id: Type.Number() })),
	),
);
const retyped = chord.remote("t.chat", {
	...chatV1.def,
	variants: { ...chatV1.def.variants, viewer: { state: { view: Type.Object({ text: Type.Number() }) } } },
});
const chatV2 = chord.remote("t.chat", { ...chatV1.def, version: 2, variants: { viewer: { state: { body: View } } } });
const moved = chord.remote("t.chat", { ...chatV1.def, host: chord.hostNamed("server") });
const counter = chord.remote("t.counter", {
	version: 1,
	input: Type.Object({ name: Type.String() }),
	host: chord.hostNamed("server"),
	variants: {
		main: {
			state: { count: Type.Object({ n: Type.Number() }) },
			methods: { add: chord.method(Type.Number(), Type.Number()), who: chord.method(undefined, Type.Unknown()) },
		},
	},
});
const index = chord.local("t.idx", {
	version: 1,
	input: Type.Object({}),
	members: {
		harness: chord.opaque("h@1"),
		onChange: chord.method(Type.Function([Type.String()], Type.Void())),
		progress: Type.Object({ ready: Type.Boolean() }),
		"10": chord.method(),
		"2": chord.method(undefined, Type.Integer({ minimum: 1 })),
	},
});
const index2 = chord.local("t.idx", { ...index.def, members: { ...index.def.members, harness: chord.opaque("h@2") } });
const methods = (members: Record<string, unknown>) =>
	chord.local("t.methods", { version: 1, input: Type.Object({}), members });
const contracts: Record<string, any> = {
	chatV1,
	additive,
	required,
	retyped,
	chatV2,
	moved,
	counter,
	index,
	index2,
	none: methods({ m: chord.method() }),
	withArgs: methods({ m: chord.method(Type.String()) }),
	withResult: methods({ m: chord.method(undefined, Type.Number()) }),
};

/** The definition as data (member kinds read from the description), so the Rust test builds it through its API. */
function defOf(contract: any): unknown {
	const def = contract.def;
	const members = (kind: string, names: string[]) =>
		names.map((name) => [name, plain(contract.description.variants[kind][name])]);
	return {
		scope: contract.scope,
		name: contract.name,
		version: def.version,
		input: plain(def.input),
		host: def.host === undefined ? undefined : plain(def.host),
		variants:
			contract.scope === "remote"
				? Object.entries(def.variants).map(([kind, variant]: [string, any]) => ({
						kind,
						state: members(kind, Object.keys(variant.state ?? {})),
						methods: members(kind, Object.keys(variant.methods ?? {})),
					}))
				: undefined,
		members: contract.scope === "local" ? members("", Object.keys(def.members ?? {})) : undefined,
	};
}

const pairs = [
	["additive", "chatV1"],
	["chatV1", "additive"],
	["required", "chatV1"],
	["retyped", "chatV1"],
	["chatV2", "chatV1"],
	["moved", "chatV1"],
	["counter", "chatV1"],
	["index2", "index"],
	["none", "none"],
	["withArgs", "none"],
	["none", "withArgs"],
	["withResult", "none"],
	["none", "withResult"],
];
const bound = (provider: any, accepts: unknown, consumer: any) => {
	try {
		return chord.bind(provider, accepts, consumer) === undefined ? "direct" : "adapter";
	} catch (error: any) {
		return { code: error.code, message: error.message };
	}
};
write("contracts.json", {
	contracts: Object.entries(contracts).map(([label, contract]) => ({
		label,
		def: defOf(contract),
		description: JSON.stringify(contract.description),
		canonical: schema.canonicalJson(contract.description),
		fingerprint: contract.fingerprint,
	})),
	coverage: pairs.map(([provider, consumer]) => ({
		provider,
		consumer,
		coverage: chord.coverage(contracts[provider].description, contracts[consumer].description) ?? null,
		bind: bound(contracts[provider], { 1: () => ({}) }, contracts[consumer]),
		bindWithoutAdapters: bound(contracts[provider], undefined, contracts[consumer]),
	})),
});

// ─── schema checks ───────────────────────────────────────────────────────────

const fitsCases = [
	[Type.Function([Type.Unknown()], Type.Void()), Type.Function([Type.String()], Type.Void())],
	[Type.Function([Type.String()], Type.Void()), Type.Function([Type.Unknown()], Type.Void())],
	[Type.String(), Type.String({ not: { const: "x" } })],
	[Type.Array(Type.Number()), Type.Array(Type.Number(), { contains: Type.Literal(1) })],
	[Type.Object({ a: Type.String() }), Type.Object({ a: Type.String() }, { additionalProperties: Type.Number() })],
	[Type.Object({ x: Type.String() }), Type.Object({}, { patternProperties: { "^x$": Type.Number() } })],
	[Type.Number(), Type.Intersect([Type.Number(), Type.Number({ minimum: 1 })])],
	[
		Type.Object({ a: Type.String() }, { additionalProperties: Type.Number() }),
		Type.Object({ a: Type.String(), b: Type.Optional(Type.String()) }),
	],
	[Type.Object({ a: Type.String() }), Type.Object({ a: Type.String(), b: Type.Optional(Type.Number()) })],
	[Type.Object({ a: Type.String(), extra: Type.Number() }), Type.Object({ a: Type.String() })],
	[Type.Integer(), Type.Number()],
	[Type.Number(), Type.Integer()],
	[Type.Literal("a"), Type.String()],
	[
		Type.Union([Type.Literal("a"), Type.Literal("b")]),
		Type.Union([Type.Literal("a"), Type.Literal("b"), Type.Literal("c")]),
	],
	[Type.Union([Type.Literal("a"), Type.Literal("d")]), Type.Union([Type.Literal("a"), Type.Literal("b")])],
	[Type.Literal("a"), Type.Enum(["a", "b"])],
	[Type.Enum(["a", "b"]), Type.Enum(["a"])],
	[Type.String({ description: "doc" }), Type.String({ title: "other" })],
	[Type.String({ minLength: 1 }), Type.String()],
	[Type.String(), Type.String({ minLength: 1 })],
	[Type.Array(Type.String()), Type.Array(Type.Union([Type.String(), Type.Number()]))],
	[Type.Tuple([Type.String(), Type.Number()]), Type.Tuple([Type.String(), Type.Number()])],
	[Type.Tuple([Type.String()]), Type.Array(Type.String())],
	[Type.Unknown(), Type.String()],
	[Type.String(), Type.Unknown()],
	[
		Type.Object({ a: Type.String() }, { additionalProperties: false }),
		Type.Object({ a: Type.String() }, { additionalProperties: false }),
	],
	[Type.Object({ a: Type.String() }), Type.Object({ a: Type.String() }, { additionalProperties: false })],
	[
		Type.Object({ a: Type.String(), b: Type.String() }, { additionalProperties: false }),
		Type.Object({ a: Type.String() }, { additionalProperties: false }),
	],
	[Type.Record(Type.String(), Type.Number()), Type.Record(Type.String(), Type.Number())],
	[
		Type.Object({ x: Type.Number() }, { additionalProperties: false }),
		Type.Object({}, { patternProperties: { "^x$": Type.Number() } }),
	],
	[Type.Null(), Type.Union([Type.Null(), Type.String()])],
	[Type.Object({}), Type.Object({ a: Type.Optional(Type.String()) })],
	[Type.Object({}), Type.Object({ a: Type.String() })],
	[Type.Function([], Type.String()), Type.Function([Type.String()], Type.Unknown())],
	[Type.Function([Type.String(), Type.Number()], Type.String()), Type.Function([Type.String()], Type.String())],
];
const union = (...branches: unknown[]) => Type.Object({ u: Type.Union(branches) });
const normalizeCases = [
	[chatV1.input, { session: "s", extra: 1 }],
	[chatV1.input, { conversation: 0, session: "s" }],
	[chatV1.input, { conversation: 3, session: "s" }],
	[chatV1.input, { session: 1 }],
	[chatV1.input, {}],
	[chatV1.input, null],
	[Type.Object({}), {}],
	[
		Type.Object({
			a: Type.Optional(Type.Number({ default: 1 })),
			b: Type.Array(Type.Object({ c: Type.String({ default: "x" }) })),
		}),
		{ b: [{}, { c: "y", z: 1 }] },
	],
	[Type.Object({ o: Type.Object({ x: Type.Number({ default: 1 }) }, { default: {} }) }), {}],
	[Type.Object({ list: Type.Array(Type.Number(), { default: [1, 2] }) }), {}],
	[union(Type.Object({ a: Type.String() }), Type.Object({ b: Type.Number() })), { u: { b: 1, c: 2 } }],
	[Type.Object({ r: Type.Record(Type.String(), Type.Object({ a: Type.String() })) }), { r: { k: { a: "x", z: 1 } } }],
	[Type.Object({ a: Type.String() }, { additionalProperties: Type.Number() }), { a: "x", z: 1, y: "s" }],
	[Type.Object({ t: Type.Tuple([Type.Object({ a: Type.String() }), Type.Number()]) }), { t: [{ a: "x", q: 1 }, 2, 3] }],
	[Type.Object({ a: Type.String({ minLength: 2 }), n: Type.Integer({ minimum: 1 }) }), { a: "x", n: 0.5 }],
	[Type.Object({ a: Type.String(), b: Type.String(), c: Type.String(), d: Type.String() }), {}],
	[
		union(Type.Object({ a: Type.String() }), Type.Object({ a: Type.String(), b: Type.Number() })),
		{ u: { a: "x", b: 1 } },
	],
];
const normalized = (target: unknown, value: unknown) => {
	try {
		const result = schema.normalize(target, value, "input");
		return { value: result, identity: schema.canonicalIdentity(target, result) };
	} catch (error: any) {
		return { error: { code: error.code, reason: error.reason, message: error.message } };
	}
};
const validCases = [
	[Type.String(), 5],
	[Type.Object({ a: Type.String(), n: Type.Number() }), { a: 1, n: "x" }],
	[Type.Object({ a: Type.String(), b: Type.String() }), {}],
	[Type.Array(Type.Integer()), [1, 2.5, "x", 4.5]],
	[Type.Object({ a: Type.Object({ b: Type.Array(Type.String()) }) }), { a: { b: ["x", 2] } }],
	[Type.Union([Type.String(), Type.Number()]), true],
	[Type.Object({ a: Type.String() }, { additionalProperties: false }), { a: "x", b: 1 }],
	[Type.Literal("x"), "y"],
	[Type.Number({ minimum: 1, maximum: 3 }), 5],
	[Type.Object({ ok: Type.Boolean() }), { ok: true }],
];
const asserted = (target: unknown, value: unknown) => {
	try {
		schema.assertValid("state", "state s", target, value);
		return null;
	} catch (error: any) {
		return { code: error.code, reason: error.reason, message: error.message };
	}
};
write("schema.json", {
	fits: fitsCases.map(([sub, sup]) => ({ sub: plain(sub), sup: plain(sup), fits: schema.fits(sub, sup) })),
	normalize: normalizeCases.map(([target, value]) => ({
		schema: plain(target),
		input: value,
		...normalized(target, value),
	})),
	assertValid: validCases.map(([target, value]) => ({ schema: plain(target), value, error: asserted(target, value) })),
});

await writeWireFixtures({ upstream, write, plain, chatV1, index });
console.log(`wrote fixtures to ${out}`);
