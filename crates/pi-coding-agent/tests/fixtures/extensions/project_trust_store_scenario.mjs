// Runs `ProjectTrustStore` over temporary agent directories, on upstream under Node (expected output) and in the
// extension host. Paths read `<root>/...` with `/`; engine JSON.parse text is kept apart in `engine` fields.
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmdirSync, rmSync, statSync, symlinkSync,
	utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export async function scenario(c, options = {}) {
	const root = realpathSync(mkdtempSync(join(options.tempParent ?? tmpdir(), "pi-trust-store-scenario-")));
	const savedHome = ["HOME", "USERPROFILE"].map((name) => [name, name in process.env, process.env[name]]);
	const out = {};
	let agents = 0;
	const clean = (text) => String(text).split(root).join("<root>").replaceAll("\\", "/");
	const agentDir = () => join(root, `agent-${agents++}`);
	const paths = (dir) => ({ trust: join(dir, "trust.json"), lock: join(dir, "trust.json.lock") });
	const bytes = (dir) => (existsSync(paths(dir).trust) ? clean(readFileSync(paths(dir).trust, "utf8")) : null);
	const fields = (error) => ({ message: clean(error?.message), code: error?.code ?? null, file: error?.file ? clean(error.file) : null });
	const entry = (value) => (value === null ? null : { path: clean(value.path), decision: value.decision });
	const attempt = (fn) => {
		try {
			return { value: fn() ?? null };
		} catch (error) {
			return { error: fields(error) };
		}
	};
	const timed = (fn) => {
		const started = Date.now();
		const result = attempt(fn);
		return { ...result, waitedAtLeast150ms: Date.now() - started >= 150 };
	};
	const state = (dir) => ({ agentDir: existsSync(dir), lock: existsSync(paths(dir).lock), file: bytes(dir) });
	const parseError = (text) => {
		const dir = agentDir();
		mkdirSync(dir, { recursive: true });
		writeFileSync(paths(dir).trust, text);
		const prefix = `Failed to read trust store ${paths(dir).trust}: `;
		const result = attempt(() => new c.ProjectTrustStore(dir).get(root));
		const message = result.error ? result.error.message : null;
		const known = message?.startsWith(clean(prefix));
		return { prefixed: known, prefix: clean(prefix), engine: known ? message.slice(clean(prefix).length) : message, after: state(dir) };
	};
	const invalid = (text) => {
		const dir = agentDir();
		mkdirSync(dir, { recursive: true });
		writeFileSync(paths(dir).trust, text);
		return { ...attempt(() => new c.ProjectTrustStore(dir).get(root)), after: state(dir) };
	};
	try {
		const home = join(root, "home");
		mkdirSync(home, { recursive: true });
		process.env.HOME = home;
		if (process.platform === "win32") process.env.USERPROFILE = home;
		const work = join(root, "work", "a", "b");
		mkdirSync(work, { recursive: true });

		let dir = agentDir();
		out["get on a missing store"] = { value: new c.ProjectTrustStore(dir).get(work), after: state(dir) };
		out["getEntry on a missing store"] = { value: entry(new c.ProjectTrustStore(dir).getEntry(work)), after: state(dir) };

		dir = agentDir();
		let store = new c.ProjectTrustStore(dir);
		store.set(work, true);
		out["set true, then get"] = { value: store.get(work), entry: entry(store.getEntry(work)), after: state(dir) };
		store.set(work, false);
		out["set false replaces true"] = { value: store.get(work), after: state(dir) };
		store.set(work, null);
		out["set null removes the key"] = { value: store.get(work), after: state(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		const parent = join(root, "work", "a");
		store.setMany([{ path: parent, decision: true }, { path: work, decision: false }]);
		out["the nearest ancestor decides"] = { child: entry(store.getEntry(work)), deeper: entry(store.getEntry(join(work, "c", "d"))), after: state(dir) };
		store.setMany([{ path: parent, decision: true }, { path: work, decision: null }]);
		out["setMany: parent true, child null removes the child"] = { child: entry(store.getEntry(work)), after: state(dir) };

		dir = agentDir();
		mkdirSync(dir, { recursive: true });
		writeFileSync(paths(dir).trust, `${JSON.stringify({ [parent]: true, [work]: null, [join(root, "kept")]: null })}\n`);
		store = new c.ProjectTrustStore(dir);
		out["a null key is skipped to the parent"] = { entry: entry(store.getEntry(work)), value: store.get(work) };
		store.set(join(root, "other"), true);
		out["existing nulls are kept when writing"] = { after: state(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		const real = join(root, "real-project");
		mkdirSync(real, { recursive: true });
		const link = join(root, "linked-project");
		let linked = "skipped: no symlink";
		try {
			symlinkSync(real, link, "dir");
			linked = null;
		} catch {}
		if (linked === null) {
			store.set(link, true);
			linked = { viaLink: store.get(link), viaReal: entry(store.getEntry(real)), after: state(dir) };
		}
		out["a symlinked cwd is stored by its canonical path"] = linked;
		store.set("~/tilde-project", true);
		store.set(`${join(root, "work", "a")}/./b/../dots/`, false);
		out["~ and dot segments are normalized"] = { tilde: store.get(join(home, "tilde-project")), dots: store.get(join(root, "work", "a", "dots")), after: state(dir) };
		const tildeStore = new c.ProjectTrustStore("~/tilde-agent");
		out["the trustPath property and the class shape"] = {
			keys: Object.keys(tildeStore),
			trustPath: clean(tildeStore.trustPath),
			prototype: Object.getOwnPropertyNames(c.ProjectTrustStore.prototype).sort(),
		};

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		const astral = join(root, "keys", "\u{1F600}");
		const bmp = join(root, "keys", "�");
		store.setMany([{ path: bmp, decision: true }, { path: astral, decision: false }, { path: join(root, "keys", "B"), decision: true }, { path: join(root, "keys", "a"), decision: true }]);
		out["keys are written in UTF-16 order"] = { after: state(dir) };

		dir = agentDir();
		mkdirSync(dir, { recursive: true });
		writeFileSync(paths(dir).trust, `﻿${JSON.stringify({ [work]: true })}`);
		out["one BOM is removed"] = { value: new c.ProjectTrustStore(dir).get(work) };

		out["malformed JSON"] = parseError("{bad");
		out["two BOMs"] = parseError("\uFEFF\uFEFF{}");
		out["an empty file"] = parseError("");
		out["engine JSON.parse text"] = { bad: attempt(() => JSON.parse("{bad")).error.message, empty: attempt(() => JSON.parse("")).error.message };
		out["an array"] = invalid("[]");
		out["null"] = invalid("null");
		out["a number"] = invalid("5");
		out["a string value"] = invalid(JSON.stringify({ [work]: "yes" }));
		out["a number value"] = invalid(JSON.stringify({ [work]: 1 }));

		dir = agentDir();
		mkdirSync(paths(dir).lock, { recursive: true });
		out["a fresh foreign lock gives ELOCKED"] = { ...timed(() => new c.ProjectTrustStore(dir).get(work)), after: state(dir) };
		const past = (Date.now() - 20000) / 1000;
		utimesSync(paths(dir).lock, past, past);
		out["a stale lock is reclaimed"] = { ...timed(() => new c.ProjectTrustStore(dir).get(work)), after: state(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		let seen = null;
		const probe = { get path() {
			const info = statSync(paths(dir).lock);
			seen = { lock: true, mtimeMsModulo1000: Math.round(info.mtimeMs) % 1000 };
			return work;
		}, decision: true };
		out["the lock is held while a caller getter runs"] = { ...attempt(() => store.setMany([probe])), seen, after: state(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		out["a throwing getter releases the lock"] = { ...attempt(() => store.setMany([{ get path() { throw new Error("boom"); }, decision: true }])), after: state(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		let inner = null;
		const nested = { get path() {
			inner = timed(() => store.get(work));
			return work;
		}, decision: true };
		out["a nested call while the lock is held gives ELOCKED"] = { ...attempt(() => store.setMany([nested])), inner, after: state(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		const removeOnly = { get path() {
			rmdirSync(paths(dir).lock);
			return work;
		}, decision: true };
		out["a caller that removes the lock dir"] = { ...attempt(() => store.setMany([removeOnly])), after: state(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		const filled = { get path() {
			writeFileSync(join(paths(dir).lock, "intruder"), "x");
			return work;
		}, decision: true };
		const fill = attempt(() => store.setMany([filled]));
		const afterFill = state(dir);
		const lockedAfterFill = timed(() => store.get(work));
		utimesSync(paths(dir).lock, past, past);
		const staleFilled = attempt(() => store.get(work));
		out["a file in the lock dir: ENOTEMPTY on release, then on stale removal"] = { fill, afterFill, lockedAfterFill, staleFilled, after: state(dir) };
	} finally {
		for (const [name, had, value] of savedHome) {
			if (had) process.env[name] = value;
			else delete process.env[name];
		}
		rmSync(root, { recursive: true, force: true });
	}
	return out;
}
