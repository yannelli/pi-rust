// Oracle generator: runs every case against the authentic upstream SettingsManager at 35dfa757 and
// writes the observed results. Run only through `node --import ./register.mjs run.mjs <run dir> <out>`.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { FILE_CASES } from "../data/file-cases.mjs";
import { GLOBAL, INVALID, PROJECT } from "../data/fixtures.mjs";
import { LIFECYCLE } from "../data/lifecycle.mjs";
import { ENV_ALT, ENV_GETTERS, ENV_SET, EXTERNAL, GETTERS, MUTATED, SETTERS } from "../data/members.mjs";
import { build, normalize, plain, recorder, run, snapshot, tree } from "./harness.mjs";

const [runDir, outFile] = process.argv.slice(2);
const COPY = process.env.ORACLE_COPY;
if (!runDir || !outFile || !COPY) throw new Error("usage: run.mjs <run dir> <out file>, with ORACLE_COPY set");
const { members } = JSON.parse(readFileSync(new URL("../data/public-members.json", import.meta.url), "utf8"));
const api = await import(pathToFileURL(join(COPY, "packages/coding-agent/src/core/settings-manager.ts")).href);
const ENV_KEYS = ["VISUAL", "EDITOR", "PI_CLEAR_ON_SHRINK", "PI_HARDWARE_CURSOR", "PI_CODING_AGENT_DIR"];
const coverage = new Map(members.map((member) => [member.name, []]));
const cases = [];

async function runCase(id, spec, ops, env = {}, extra = {}, base = undefined) {
	const used = new Set();
	const saved = Object.fromEntries(ENV_KEYS.map((key) => [key, process.env[key]]));
	for (const key of ENV_KEYS) delete process.env[key];
	Object.assign(process.env, env);
	try {
		let built;
		try {
			built = build(api, spec, used);
		} catch (error) {
			cases.push({ id, env, built: { threw: error.message } });
			return;
		}
		const state = { raw: built.manager, manager: recorder(built.manager, used), storage: built.storage, base };
		state.treeAtStart = base ? tree(base) : undefined;
		const steps = await run(ops, state);
		await built.manager.flush();
		cases.push({ id, env, ...extra, steps, final: plain(snapshot(state)) });
	} finally {
		for (const name of used) coverage.get(name)?.push(id);
		for (const key of ENV_KEYS) {
			if (saved[key] === undefined) delete process.env[key];
			else process.env[key] = saved[key];
		}
	}
}

const recording = (storage, options) => ({ kind: "recording", storage, options });
const SCENARIOS = {
	empty: { kind: "inMemory", settings: {} },
	global: { kind: "inMemory", settings: GLOBAL },
	layered: recording({ global: GLOBAL, project: PROJECT }),
	invalid: { kind: "inMemory", settings: INVALID },
};

for (const [name, ...argLists] of GETTERS) {
	const ops = (argLists.length ? argLists : [[]]).map((args) => [name, ...args]);
	for (const [scenario, spec] of Object.entries(SCENARIOS)) await runCase(`getter/${name}/${scenario}`, spec, ops);
	if (ENV_GETTERS.includes(name)) {
		await runCase(`getter/${name}/env-set`, SCENARIOS.empty, ops, ENV_SET);
		await runCase(`getter/${name}/env-alt`, SCENARIOS.empty, ops, ENV_ALT);
		await runCase(`getter/${name}/env-set-with-settings`, SCENARIOS.global, ops, ENV_SET);
	}
}

for (const name of MUTATED) {
	await runCase(`copy/${name}`, SCENARIOS.layered, [["$mutateReturn", name], [name]]);
}

for (const [name, argLists, read, project] of SETTERS) {
	const scope = project ? "project" : "global";
	for (const [index, args] of argLists.entries()) {
		await runCase(`setter/${name}/${index}`, recording({ global: { unrelated: 1 }, project: { unrelatedProject: 1 } }), [
			[name, ...args],
			read,
			["$assertNoWrite", "before-snapshot"],
			["$snapshot"],
			["flush"],
			["$snapshot"],
		]);
	}
	await runCase(`setter/${name}/external-edit`, recording({ global: GLOBAL, project: PROJECT }), [
		[name, ...argLists[0]],
		["$assertNoWrite", "before-external"],
		["$external", scope, EXTERNAL[scope]],
		["$assertNoWrite", "after-external"],
		["flush"],
		["$snapshot"],
	]);
	await runCase(`setter/${name}/in-memory`, SCENARIOS.global, [[name, ...argLists[0]], read]);
	const base = join(runDir, `setter-file-${name}`);
	await run(
		[
			["$file:write", "agent/settings.json", JSON.stringify(GLOBAL, null, 2)],
			["$file:write", "cwd/.pi/settings.json", JSON.stringify(PROJECT, null, 2)],
		],
		{ base },
	);
	const fileSpec = { kind: "create", cwd: join(base, "cwd"), agentDir: join(base, "agent") };
	const fileOps = [[name, ...argLists[0]], read, ["$assertNoWrite", "before-flush"], ["flush"], ["$tree"]];
	await runCase(`setter/${name}/file`, fileSpec, fileOps, {}, {}, base);
	if (project) {
		await runCase(`setter/${name}/untrusted`, recording({ global: GLOBAL, project: PROJECT }, { projectTrusted: false }), [
			[name, ...argLists[0]],
			["$snapshot"],
		]);
	}
}

for (const item of LIFECYCLE) {
	const spec = { ...item.manager };
	const extra = item.note ? { note: item.note } : {};
	if (item.checkInput) {
		spec.settings = structuredClone(item.checkInput);
		spec.noClone = true;
	}
	await runCase(`lifecycle/${item.id}`, spec, item.ops, {}, extra);
	if (item.checkInput) {
		cases.at(-1).inputUnchanged = JSON.stringify(spec.settings) === JSON.stringify(item.checkInput);
	}
}

for (const item of FILE_CASES) {
	const base = join(runDir, `file-${item.id}`);
	mkdirSync(join(base, "cwd"), { recursive: true });
	mkdirSync(join(base, "agent"), { recursive: true });
	await run(item.seed.map(([kind, ...args]) => [`$file:${kind}`, ...args]), { base });
	const spec = { kind: "create", cwd: join(base, "cwd"), agentDir: item.defaultAgentDir ? undefined : join(base, "agent"), options: item.options };
	const env = item.defaultAgentDir ? { PI_CODING_AGENT_DIR: join(base, "agent") } : {};
	await runCase(`file/${item.id}`, spec, item.ops, env, { seeded: tree(base) }, base);
}

// CommonJS packages load through require(), which the module hooks do not see; list them here.
const cjs = Object.keys(createRequire(import.meta.url)("node:module")._cache).sort();
const outsideCopy = cjs.filter((path) => !path.startsWith(`${COPY}/`));
if (outsideCopy.length) throw new Error(`oracle: CommonJS modules outside the run copy: ${outsideCopy.join(", ")}`);
const uncovered = [...coverage].filter(([, ids]) => ids.length < 2).map(([name]) => name);
const output = normalize(
	{
		source: { repository: "earendil-works/pi", commit: "35dfa757378bd7a8570670e99c2687bdd1039a06" },
		runtime: { node: process.version, platform: process.platform },
		memberCount: members.length,
		commonJsLoaded: cjs.map((path) => path.slice(COPY.length + 1)),
		cases,
	},
	[
		[runDir, "<RUN>"],
		[process.env.HOME, "<HOME>"],
		[COPY, "<COPY>"],
	],
);
const text = JSON.stringify(output, null, 1);
if (uncovered.length) throw new Error(`oracle: members with fewer than 2 behavior cases: ${uncovered.join(", ")}`);
writeFileSync(outFile, `${text}\n`);
const map = Object.fromEntries([...coverage].map(([name, ids]) => [name, ids]));
writeFileSync(`${outFile}.coverage.json`, `${JSON.stringify(map, null, 1)}\n`);
console.log(`oracle: ${cases.length} cases, ${members.length} members covered`);
