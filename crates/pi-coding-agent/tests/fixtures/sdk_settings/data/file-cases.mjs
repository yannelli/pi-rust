// FileSettingsStorage cases through `SettingsManager.create`, with real files and the lock-pinned
// proper-lockfile 4.1.2. Each case gets `<base>/cwd` and `<base>/agent`; paths below are under <base>.
import { GLOBAL, LEGACY, PROJECT } from "./fixtures.mjs";

const json = (value) => JSON.stringify(value, null, 2);
const G = "agent/settings.json";
const P = "cwd/.pi/settings.json";

/** `{ id, seed: [[file step, path, value]], options?, defaultAgentDir?, ops }`. */
export const FILE_CASES = [
	{ id: "empty-dirs", seed: [], ops: [["getSettings"], ["isProjectTrusted"], ["drainErrors"], ["$tree"]] },
	{ id: "untrusted-option", seed: [["write", P, json(PROJECT)]], options: { projectTrusted: false }, ops: [["isProjectTrusted"], ["getProjectSettings"], ["$tree"]] },
	{ id: "existing-files-read", seed: [["write", G, json(GLOBAL)], ["write", P, json(PROJECT)]], ops: [["getGlobalSettings"], ["getProjectSettings"], ["getSettings"], ["drainErrors"], ["$tree"]] },
	{ id: "global-write-existing", seed: [["write", G, json({ unrelated: 1, compaction: { reserveTokens: 5 } })]], ops: [["setDefaultModel", "file-model"], ["setCompactionEnabled", false], ["$assertNoWrite", "before-flush"], ["$tree"], ["flush"], ["$tree"]] },
	{ id: "global-write-creates-dir", seed: [], ops: [["setTheme", "light"], ["$assertNoWrite", "before-flush"], ["flush"], ["$tree"]] },
	{ id: "yield-lets-queued-write-run", seed: [["write", G, json({ a: 1 })]], ops: [["setDefaultModel", "y"], ["$assertNoWrite", "before-yield"], ["$yield"], ["$tree"], ["$yield"], ["$yield"], ["$yield"], ["$tree"], ["flush"], ["$tree"]] },
	{ id: "project-write-creates-config-dir", seed: [], ops: [["setProjectSkillPaths", ["./s"]], ["flush"], ["$tree"]] },
	{ id: "project-write-untrusted", seed: [["write", P, json(PROJECT)]], options: { projectTrusted: false }, ops: [["setProjectSkillPaths", ["./s"]], ["flush"], ["$tree"]] },
	{ id: "external-edit-before-flush", seed: [["write", G, json({ a: 1 })]], ops: [["setDefaultProvider", "p"], ["$assertNoWrite", "before-external"], ["$file:write", G, json({ a: 2, external: true })], ["flush"], ["$file:read", G]] },
	{ id: "bom-file", seed: [["write", G, `﻿${json({ defaultModel: "bom" })}`]], ops: [["getDefaultModel"], ["setDefaultProvider", "x"], ["flush"], ["$file:read", G]] },
	{ id: "legacy-file", seed: [["write", G, json(LEGACY)]], ops: [["getGlobalSettings"], ["setDefaultModel", "m"], ["flush"], ["$file:read", G]] },
	{ id: "malformed-global-file", seed: [["write", G, "{ nope"]], ops: [["getSettings"], ["drainErrors"], ["setDefaultModel", "m"], ["flush"], ["$file:read", G], ["drainErrors"]] },
	{ id: "malformed-project-file", seed: [["write", P, "[1,"]], ops: [["getSettings"], ["drainErrors"]] },
	{ id: "settings-path-is-directory", seed: [["mkdir", G]], ops: [["getSettings"], ["drainErrors"]] },
	{ id: "held-lock-on-read", seed: [["write", G, json({ defaultModel: "locked" })], ["mkdir", `${G}.lock`]], ops: [["getDefaultModel"], ["drainErrors"], ["$tree"]] },
	{ id: "held-lock-on-write", seed: [["write", G, json({ a: 1 })]], ops: [["$file:mkdir", `${G}.lock`], ["setDefaultModel", "m"], ["$snapshot"], ["flush"], ["drainErrors"], ["$file:read", G]] },
	{ id: "stale-lock-taken-over", seed: [["write", G, json({ a: 1 })], ["mkdir", `${G}.lock`], ["age", `${G}.lock`, 3600]], ops: [["getSettings"], ["setDefaultModel", "m"], ["flush"], ["drainErrors"], ["$tree"]] },
	{ id: "reload-after-external-file-edit", seed: [["write", G, json({ defaultModel: "a" })]], ops: [["$file:write", G, json({ defaultModel: "b" })], ["reload"], ["getDefaultModel"]] },
	{ id: "trust-false-to-true-reads-project-file", seed: [["write", P, json(PROJECT)]], options: { projectTrusted: false }, ops: [["setProjectTrusted", true], ["getProjectSettings"], ["$tree"]] },
	{ id: "default-agent-dir-from-env", seed: [["write", G, json({ defaultModel: "env-agent" })]], defaultAgentDir: true, ops: [["getDefaultModel"], ["setTheme", "t"], ["flush"], ["$file:read", G]] },
	{ id: "device-id-persisted", seed: [], ops: [["getOrCreateDeviceId"], ["$assertNoWrite", "before-flush"], ["flush"], ["$file:read", G]] },
];
