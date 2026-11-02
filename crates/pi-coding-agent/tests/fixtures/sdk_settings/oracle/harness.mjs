// Case harness around the authentic upstream SettingsManager: storages that record each
// `withLock` call, file steps for FileSettingsStorage cases, a member-call recorder, and normalization.
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

/** A custom `SettingsStorage` (upstream interface `withLock(scope, fn)`) that logs every call. */
export class RecordingStorage {
	constructor(initial = {}, faults = {}) {
		this.text = { global: initial.global, project: initial.project };
		this.faults = faults;
		this.log = [];
	}

	withLock(scope, fn) {
		const fault = this.faults[scope];
		const current = this.text[scope];
		if (fault === "throw-on-read") {
			this.log.push({ scope, current, threw: "read" });
			throw new Error(`storage read failed: ${scope}`);
		}
		const next = fn(current);
		if (next !== undefined && fault === "throw-on-write") {
			this.log.push({ scope, current, next, threw: "write" });
			throw new Error(`storage write failed: ${scope}`);
		}
		this.log.push({ scope, current, next: next === undefined ? null : next });
		if (next !== undefined) this.text[scope] = next;
	}
}

/** Builds the manager of a case. `kind` picks the upstream factory under test. */
export function build(api, spec, used) {
	const { SettingsManager, InMemorySettingsStorage } = api;
	const record = (name) => used.add(name);
	switch (spec.kind) {
		case "inMemory":
			record("inMemory");
			return { manager: SettingsManager.inMemory(spec.noClone ? spec.settings : clone(spec.settings), spec.options) };
		case "inMemoryNoArgs":
			record("inMemory");
			return { manager: SettingsManager.inMemory() };
		case "recording": {
			const storage = new RecordingStorage(text(spec.storage), spec.faults);
			record("fromStorage");
			return { manager: SettingsManager.fromStorage(storage, spec.options), storage };
		}
		case "memoryStorage": {
			const storage = new InMemorySettingsStorage();
			record("fromStorage");
			return { manager: SettingsManager.fromStorage(storage, spec.options), storage };
		}
		case "create":
			record("create");
			return { manager: SettingsManager.create(spec.cwd, spec.agentDir, spec.options) };
		default:
			throw new Error(`unknown manager kind ${spec.kind}`);
	}
}

const clone = (value) => (value === undefined ? undefined : structuredClone(value));

/** Storage seeds: objects become `JSON.stringify(value, null, 2)`, strings are taken as given. */
function text(storage = {}) {
	const out = {};
	for (const scope of ["global", "project"]) {
		const value = storage[scope];
		out[scope] = value === undefined || typeof value === "string" ? value : JSON.stringify(value, null, 2);
	}
	return out;
}

/** Wraps `manager` so each member call is recorded once a case has used it. */
export function recorder(manager, used) {
	return new Proxy(manager, {
		get(target, property, receiver) {
			const value = Reflect.get(target, property, receiver);
			if (typeof property !== "string" || typeof value !== "function") return value;
			return (...args) => {
				used.add(property);
				return value.apply(target, args);
			};
		},
	});
}

/** Runs `ops` in order. A member call stays synchronous unless it returns a thenable, so a queued
 * upstream write cannot run between steps unless a `$yield`, `flush`, or `reload` step awaits. */
export async function run(ops, state) {
	const steps = [];
	for (const [member, ...args] of ops) {
		if (member === "$yield") {
			await null;
			steps.push({ step: "$yield" });
			continue;
		}
		if (member === "$assertNoWrite") {
			assertNoWrite(state, args[0]);
			steps.push({ step: "$assertNoWrite", at: args[0] });
			continue;
		}
		if (member === "$external") {
			state.storage.text[args[0]] = args[1];
			steps.push({ step: "$external", scope: args[0] });
			continue;
		}
		if (member.startsWith("$file:")) {
			steps.push({ step: member, path: args[0], ...fileStep(state.base, member.slice(6), args) });
			continue;
		}
		if (member === "$tree") {
			steps.push({ step: "$tree", tree: tree(state.base) });
			continue;
		}
		if (member === "$snapshot") {
			steps.push({ step: "$snapshot", ...plain(snapshot(state)) });
			continue;
		}
		if (member === "$mutateReturn") {
			const value = state.manager[args[0]](...args.slice(1));
			if (Array.isArray(value)) value.push("__oracleMutation");
			else if (value && typeof value === "object") value.__oracleMutation = true;
			steps.push({ step: "$mutateReturn", member: args[0] });
			continue;
		}
		const threw = (error) => ({ member, args: plain(args), threw: { message: error.message, code: error.code ?? null } });
		let value;
		try {
			value = state.manager[member](...args);
		} catch (error) {
			steps.push(threw(error));
			continue;
		}
		if (value === null || typeof value?.then !== "function") {
			steps.push({ member, args: plain(args), returned: plain(value) });
			continue;
		}
		try {
			steps.push({ member, args: plain(args), awaited: true, returned: plain(await value) });
		} catch (error) {
			steps.push({ ...threw(error), awaited: true });
		}
	}
	return steps;
}

/** Fails the run when storage was written since the case started or the last snapshot: a storage
 * log entry with a `next` text, or a changed file tree under the case's base folder. */
function assertNoWrite(state, label) {
	const writes = state.storage?.log?.filter((entry) => typeof entry.next === "string") ?? [];
	if (writes.length) throw new Error(`oracle harness: storage written before ${label}: ${JSON.stringify(writes)}`);
	if (state.base && JSON.stringify(tree(state.base)) !== JSON.stringify(state.treeAtStart)) {
		throw new Error(`oracle harness: files changed before ${label}`);
	}
}

/** File steps on paths under a file case's base folder: `write`, `read`, `mkdir`, and `age`. */
function fileStep(base, kind, [relative, value]) {
	const path = join(base, relative);
	if (kind === "write") {
		mkdirSync(dirname(path), { recursive: true });
		writeFileSync(path, value);
		return {};
	}
	if (kind === "mkdir") {
		mkdirSync(path, { recursive: true });
		return {};
	}
	if (kind === "age") {
		const seconds = Date.now() / 1000 - value;
		utimesSync(path, seconds, seconds);
		return { ageSeconds: value };
	}
	if (kind === "read") return existsSync(path) ? { text: readFileSync(path, "utf8") } : { missing: true };
	throw new Error(`unknown file step ${kind}`);
}

/** Every entry under `base`: folders, and files with their text. Lock folder mtimes are left out. */
export function tree(base) {
	const out = {};
	const walk = (dir, prefix) => {
		for (const name of readdirSync(dir).sort()) {
			const path = join(dir, name);
			const relative = prefix ? `${prefix}/${name}` : name;
			if (statSync(path).isDirectory()) {
				out[`${relative}/`] = "dir";
				walk(path, relative);
			} else {
				out[relative] = readFileSync(path, "utf8");
			}
		}
	};
	if (existsSync(base)) walk(base, "");
	return out;
}

/** Layers, merged settings, errors, and storage traffic, read through the unrecorded manager. It consumes:
 * it drains `drainErrors` and the storage log, and moves the `$assertNoWrite` file baseline forward. */
export function snapshot(state) {
	const { raw: manager, storage } = state;
	if (state.base) state.treeAtStart = tree(state.base);
	return {
		global: manager.getGlobalSettings(),
		project: manager.getProjectSettings(),
		merged: manager.getSettings(),
		trusted: manager.isProjectTrusted(),
		errors: manager.drainErrors().map(({ scope, path, error }) => ({
			scope,
			path: path ?? null,
			message: error.message,
			isError: error instanceof Error,
		})),
		storage: storage?.text ? { ...storage.text } : null,
		storageLog: storage?.log ? storage.log.splice(0) : null,
		files: state.base ? tree(state.base) : null,
		consumed: ["drainErrors", "storageLog", "fileBaseline"],
	};
}

/** A JSON copy taken at once, so later mutation of a live return value cannot change the record. */
export function plain(value) {
	if (value === undefined) return { undefined: true };
	return JSON.parse(
		JSON.stringify(value, (_key, item) => {
			if (item === undefined) return { undefined: true };
			if (item instanceof Error) return { Error: item.message };
			if (typeof item === "number" && !Number.isFinite(item)) return { number: String(item) };
			return item;
		}),
	);
}

/** Replaces random UUIDs and the `[path, token]` pairs with stable tokens, in order of first appearance. */
export function normalize(value, replacements) {
	const tokens = new Map();
	const uuid = /[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/g;
	let json = JSON.stringify(plain(value));
	for (const [path, token] of replacements) json = json.split(path).join(token);
	const replaced = json.replace(uuid, (match) => {
		if (!tokens.has(match)) tokens.set(match, `<uuid#${tokens.size + 1}>`);
		return tokens.get(match);
	});
	return JSON.parse(replaced);
}
