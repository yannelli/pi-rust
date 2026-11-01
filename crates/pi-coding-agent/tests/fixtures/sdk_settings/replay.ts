// The oracle cases of `oracle/run.mjs`, unchanged, run against pi-rust's SDK `SettingsManager` by
// the `/replay` command. It writes `{ cases }` in the golden's form to `<cwd>/replay.json`.
import { mkdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import * as api from "@earendil-works/pi-coding-agent";
import { FILE_CASES } from "./data/file-cases.mjs";
import { GLOBAL, INVALID, PROJECT } from "./data/fixtures.mjs";
import { LIFECYCLE } from "./data/lifecycle.mjs";
import { ENV_ALT, ENV_GETTERS, ENV_SET, EXTERNAL, GETTERS, MUTATED, SETTERS } from "./data/members.mjs";
import { build, normalize, plain, recorder, run, snapshot, tree } from "./oracle/harness.mjs";

const ENV_KEYS = ["VISUAL", "EDITOR", "PI_CLEAR_ON_SHRINK", "PI_HARDWARE_CURSOR", "PI_CODING_AGENT_DIR"];

async function replay(runDir) {
	const cases = [];
	const used = new Map();

	async function runCase(id, spec, ops, env = {}, extra = {}, base = undefined) {
		const members = new Set();
		const saved = Object.fromEntries(ENV_KEYS.map((key) => [key, process.env[key]]));
		for (const key of ENV_KEYS) delete process.env[key];
		Object.assign(process.env, env);
		try {
			let built;
			try {
				built = build(api, spec, members);
			} catch (error) {
				cases.push({ id, env, built: { threw: error.message } });
				return;
			}
			const state = { raw: built.manager, manager: recorder(built.manager, members), storage: built.storage, base };
			state.treeAtStart = base ? tree(base) : undefined;
			const steps = await run(ops, state);
			await built.manager.flush();
			cases.push({ id, env, ...extra, steps, final: plain(snapshot(state)) });
			built.manager._sdkDispose();
		} finally {
			for (const name of members) used.set(name, (used.get(name) ?? 0) + 1);
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

	const replacements = [
		[runDir, "<RUN>"],
		[homedir(), "<HOME>"],
	];
	return normalize({ cases, used: Object.fromEntries(used) }, replacements);
}

export default function (pi) {
	pi.registerCommand("replay", {
		handler: async (_args, ctx) => {
			const runDir = join(ctx.cwd, "oracle-run");
			mkdirSync(runDir, { recursive: true });
			writeFileSync(join(ctx.cwd, "replay.json"), JSON.stringify(await replay(runDir)));
		},
	});
}
