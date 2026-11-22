// Writes jsdiff results for the Rust port in crates/pi-durable/src/diff to compare against (docs/parity/jsdiff.md).
// Usage: node scripts/jsdiff-oracle.mjs <upstream pi checkout> <jsdiff package dir> > oracle.json
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { createRequire, stripTypeScriptTypes } from "node:module";
import { join, resolve } from "node:path";

const [upstream, jsdiffDir, mode, sqlitePath] = process.argv.slice(2);
if (!upstream || !jsdiffDir || (mode !== undefined && (mode !== "--large" || !sqlitePath))) {
	console.error("usage: node scripts/jsdiff-oracle.mjs <upstream pi checkout> <jsdiff package dir> [--large <sqlite3.c>]");
	process.exit(2);
}
const Diff = createRequire(import.meta.url)(resolve(jsdiffDir));
const editDiff = readFileSync(join(upstream, "packages/coding-agent/src/core/tools/edit-diff.ts"), "utf8");
const start = editDiff.indexOf("export function generateDiffString");
const body = stripTypeScriptTypes(editDiff.slice(start, editDiff.indexOf("\n}\n", start) + 2)).replace(/^export /, "");
const generateDiffString = new Function("Diff", `${body}; return generateDiffString;`)(Diff);
const patch = (old, next, context) =>
	Diff.createTwoFilesPatch("f.txt", "f.txt", old, next, undefined, undefined, {
		context,
		headerOptions: Diff.FILE_HEADERS_ONLY,
	});

// xorshift32, also in crates/pi-durable/src/diff/tests.rs, so both sides build the same large pair.
function xorshift(seed) {
	let state = seed >>> 0;
	return (n) => {
		state ^= state << 13;
		state >>>= 0;
		state ^= state >>> 17;
		state ^= state << 5;
		state >>>= 0;
		return state % n;
	};
}

const LARGE_VOCAB = ["}", "", "  return x;", "a", "b", "\tif (x) {", "x", "é中😀"];
function largePair() {
	const next = xorshift(0x2545f491);
	const old = Array.from({ length: 3000 }, () => LARGE_VOCAB[next(8)]);
	const changed = old.map((line) => (next(10) < 4 ? LARGE_VOCAB[next(8)] : line));
	return [`${old.join("\n")}\n`, `${changed.join("\n")}\n`];
}

const rand = xorshift(Number(process.env.SEED ?? 12345));
const pick = (items) => items[rand(items.length)];

function mutate(items, vocab, ops, maxRun) {
	const next = items.slice();
	for (let k = 0; k < ops; k++) {
		const at = rand(next.length + 1);
		const op = rand(3);
		const run = 1 + rand(maxRun);
		if (op === 0) next.splice(at, 0, ...Array.from({ length: run }, () => pick(vocab)));
		else if (op === 1) next.splice(at, run);
		else if (next.length) next[Math.min(at, next.length - 1)] = pick(vocab);
	}
	return next;
}

function lineCases() {
	const cases = [];
	let small = [""];
	for (let i = 0; i < 3; i++) small = small.concat(small.flatMap((s) => ["a\n", "b\n", "\n"].map((t) => s + t)));
	const specials = ["a", "b", "a\nb", "é\r\n", "a\r\nb", "中\n", "x\t y\n", "\r", "a\rb\n", "\n\n", "a\r\n"];
	small = [...new Set(small.concat(specials))];
	for (const a of small) for (const b of small) cases.push([a, b, 4]);
	const vocab = ["}", "", "  return x;", "a", "b", "fn main() {", "    let v = 1;", "x", "é中😀", "\t//", "a\rb", " "];
	for (let i = 0; i < 5000; i++) {
		const large = i >= 4700;
		const n = 1 + rand(large ? 3000 : 120);
		const lines = Array.from({ length: n }, () => pick(vocab.slice(0, 3 + rand(vocab.length - 2))));
		const changed = large ? lines.map((line) => (rand(100) < i % 50 ? pick(vocab) : line)) : lines;
		const next = mutate(changed, vocab, rand(large ? 30 : 9), large ? 20 : 3);
		const eol = pick(["\n", "\n", "\r\n"]);
		const join = (items) => items.join(eol) + (rand(10) < 8 ? eol : "");
		cases.push([join(lines), join(next), pick([4, 4, 4, 0, 1, 2, 3, 10])]);
	}
	for (const n of [1, 2, 5, 9, 10, 20]) {
		const old = Array.from({ length: n }, (_, i) => `line ${i}\n`).join("");
		for (let i = 0; i < n; i++) {
			for (const next of [old.replace(`line ${i}\n`, "changed\n"), old.replace(`line ${i}\n`, ""), old.replace(`line ${i}\n`, `line ${i}\nextra\n`)]) {
				for (const context of [4, 0, 1]) cases.push([old, next, context]);
			}
		}
		cases.push([old, old.slice(0, -1), 4], [old.slice(0, -1), old, 4]);
	}
	return cases;
}

function wordCases() {
	const cases = [];
	const fixed = ["", " ", "  ", "a", " a", "a ", "call(a)", "call(b)", "foo bar baz", "foo baz", "foo qux baz", "foo   bar baz", "foo  baz", "\ta = 1;", "\ta = 2;"];
	for (const a of fixed) for (const b of fixed) cases.push([a, b]);
	const vocab = ["call", "(", ")", "a", "b", "foo", " ", "  ", "\t", " \t ", ".", ",", "=", "é", "中文", "😀", " ", "  ", "　", "﻿", "\u0085", "\n", "\r\n", "x_1", "42", "->", "ÿ", "×", "ǅ", "ˇ", "Ḁ", "ʰ"];
	for (let i = 0; i < 30000; i++) {
		const tokens = Array.from({ length: rand(i % 3 === 0 ? 30 : 12) }, () => pick(vocab));
		cases.push([tokens.join(""), mutate(tokens, vocab, rand(8), 2).join("")]);
	}
	return cases;
}

function largeFileCases(path) {
	const source = readFileSync(path, "utf8").split("\n").slice(0, 131072);
	if (source.length < 131072) throw Error("--large needs a sqlite3.c with at least 131072 lines");
	const blocks = Array.from({ length: 8 }, (_, i) => {
		const start = 24000 + i * 1024;
		const block = source.slice(start, start + 1024);
		block[512] += " /* moved */";
		return block;
	});
	const moved = source.slice();
	moved.splice(24000, 8192, ...blocks.reverse().flat());
	const long = "0123456789abcdef".repeat(196608);
	return [
		[long + "a\n", long + "b\n", 4],
		[source.join("\n") + "\n", moved.join("\n") + "\n", 4],
		["a\r\nb\nc\r\nd\n", "a\r\nb\r\nc\nd\n", 4],
		["a\0b\n\0c\0", "a\0b\n\0d\0", 4],
	];
}

const kind = (part) => (part.added ? "Added" : part.removed ? "Removed" : "Equal");
const inputs = lineCases();
if (mode === "--large") inputs.push(...largeFileCases(sqlitePath));
const lines = inputs.map(([old, next, context]) => {
	const { diff, firstChangedLine } = generateDiffString(old, next, context);
	return { old, new: next, context, diff, first: firstChangedLine ?? null, patch: patch(old, next, context) };
});
const words = wordCases().map(([old, next]) => ({ old, new: next, changes: Diff.diffWords(old, next).map((part) => [kind(part), part.value]) }));
const [largeOld, largeNew] = largePair();
const large = generateDiffString(largeOld, largeNew, 4);
const largeSha256 = createHash("sha256")
	.update(`${large.diff}\n${large.firstChangedLine}\n${patch(largeOld, largeNew, 4)}`)
	.digest("hex");
const version = JSON.parse(readFileSync(join(jsdiffDir, "package.json"), "utf8")).version;
process.stdout.write(JSON.stringify({ jsdiff: version, largeSha256, lines, words }));
