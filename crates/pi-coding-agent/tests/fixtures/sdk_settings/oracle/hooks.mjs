// Module hooks for the oracle run: modules resolve only to Node builtins, the run copy (pinned upstream
// sources plus the lock-pinned packages), and this prep folder; each run-copy file must match a manifest.
import { appendFileSync, readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { builtinModules } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";

const COPY = process.env.ORACLE_COPY;
const ORACLE = process.env.ORACLE_DIR;
const LOG = process.env.ORACLE_LOADED_LOG;
if (!COPY || !ORACLE || !LOG) throw new Error("oracle hooks: ORACLE_COPY, ORACLE_DIR, and ORACLE_LOADED_LOG are required");
const SOURCES = JSON.parse(readFileSync(`${ORACLE}/data/upstream-manifest.json`, "utf8")).files;
const PACKAGES = JSON.parse(readFileSync(`${ORACLE}/data/packages-manifest.json`, "utf8")).files;
const BUILTINS = new Set(builtinModules);
// The pinned workspace link points `@earendil-works/pi-ai` at packages/ai, whose exports name a built
// `dist/`; the oracle loads the git-pinned source of that one module instead.
const WORKSPACE = { "@earendil-works/pi-ai/utils/retry": "packages/ai/src/utils/retry.ts" };

const inside = (path, root) => path.startsWith(`${root}/`);

function expected(relative) {
	if (relative.startsWith("node_modules/")) return PACKAGES[relative];
	const entry = SOURCES[relative];
	return entry?.runtime ? entry.sha256 : undefined;
}

export async function resolve(specifier, context, nextResolve) {
	if (specifier.startsWith("node:") || BUILTINS.has(specifier)) return nextResolve(specifier, context);
	if (WORKSPACE[specifier]) return { url: pathToFileURL(`${COPY}/${WORKSPACE[specifier]}`).href, shortCircuit: true };
	const resolved = await nextResolve(specifier, context);
	if (!resolved.url.startsWith("file:")) throw new Error(`oracle: refused non-file module ${specifier}`);
	const path = fileURLToPath(resolved.url);
	if (!inside(path, COPY) && !inside(path, ORACLE)) throw new Error(`oracle: refused ${specifier} -> ${path}`);
	return resolved;
}

export async function load(url, context, nextLoad) {
	if (url.startsWith("file:")) {
		const path = fileURLToPath(url);
		if (inside(path, COPY)) {
			const relative = path.slice(COPY.length + 1);
			const actual = createHash("sha256").update(readFileSync(path)).digest("hex");
			if (expected(relative) !== actual) throw new Error(`oracle: ${relative} is not a manifest file (sha256 ${actual})`);
			appendFileSync(LOG, `${JSON.stringify({ file: relative, sha256: actual, via: "hook" })}\n`);
		} else if (!inside(path, ORACLE)) {
			throw new Error(`oracle: refused to load ${path}`);
		}
	}
	return nextLoad(url, context);
}
