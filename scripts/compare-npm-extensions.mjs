// Load published npm extension packages in pi-rust and in upstream pi over RPC, offline, and
// report each package whose tools, commands, errors, or stderr differ. See
// docs/parity/extension-packages.md.
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";

const OBSERVER = `export default function (pi) {
	pi.registerCommand("audit-dump", { handler: async (_args, ctx) => {
		const tools = pi.getAllTools().map((tool) => tool.name).sort();
		const commands = pi.getCommands().map((command) => command.name).sort();
		ctx.ui.notify("AUDIT " + JSON.stringify({ tools, commands }), "info");
	} });
}
`;

/** The `count` most downloaded npm packages with the `pi-package` keyword that declare extensions. */
async function top(count) {
	const found = new Map();
	for (let from = 0; from < 1000; from += 250) {
		const url = `https://registry.npmjs.org/-/v1/search?text=keywords:pi-package&size=250&from=${from}&popularity=1.0&quality=0.0&maintenance=0.0`;
		const { objects } = await (await fetch(url)).json();
		for (const { package: pkg, downloads } of objects) found.set(pkg.name, downloads?.monthly ?? 0);
	}
	const ranked = [...found].sort((a, b) => b[1] - a[1]);
	const picked = [];
	for (const [name] of ranked) {
		if (picked.length === count) break;
		const manifest = await (await fetch(`https://registry.npmjs.org/${name.replace("/", "%2f")}/latest`)).json();
		if (manifest.pi?.extensions?.length) picked.push(`${name}@${manifest.version}`);
	}
	return picked;
}

/** The package directory of `spec` (`name@version`) under its prefix in `prefixes`. */
function packageDir(prefixes, spec) {
	const name = spec.slice(0, spec.lastIndexOf("@"));
	return join(prefixes, name.replace(/[/@]/g, "_"), "node_modules", name);
}

/** One RPC run: `{ end, tools, commands, errors, stderr }`. */
async function run(command, packagePath) {
	const dir = await mkdtemp(join(tmpdir(), "pi-npm-compare-"));
	const agent = join(dir, "agent");
	const cwd = join(dir, "work");
	await mkdir(agent);
	await mkdir(cwd);
	const observer = join(dir, "observer.ts");
	await writeFile(observer, OBSERVER);
	const model = { baseUrl: "http://127.0.0.1:1/v1", api: "openai-completions", apiKey: "offline-fixture", models: [{ id: "offline" }] };
	await writeFile(join(agent, "models.json"), JSON.stringify({ providers: { audit: model } }));
	const env = {
		PATH: process.env.PATH, HOME: dir, USERPROFILE: dir, TMPDIR: dir, TMP: dir, TEMP: dir,
		XDG_CONFIG_HOME: dir, XDG_CACHE_HOME: dir, XDG_DATA_HOME: dir, PI_CODING_AGENT_DIR: agent,
		PI_OFFLINE: "1", PI_NODE_PATH: process.execPath, TERM: "dumb", GIT_CONFIG_NOSYSTEM: "1",
	};
	if (process.platform === "win32") env.SystemRoot = process.env.SystemRoot;
	const args = ["--offline", "--no-approve", "--no-session", "--model", "audit/offline", "--mode", "rpc", "-e", observer];
	if (packagePath) args.push("-e", packagePath);
	const child = spawn(command[0], [...command.slice(1), ...args], { cwd, env });
	const result = { end: undefined, tools: null, commands: null, errors: [], stderr: "" };
	child.stderr.setEncoding("utf8").on("data", (text) => { result.stderr += text; });
	const send = (record) => child.stdin.write(JSON.stringify(record) + "\n");
	send({ id: "dump", type: "prompt", message: "/audit-dump" });
	const finished = new Promise((done) => {
		const timer = setTimeout(() => {
			result.end = "timeout";
			done();
		}, 120_000);
		child.on("exit", (code) => {
			result.end ??= `exit ${code}`;
			clearTimeout(timer);
			done();
		});
		createInterface({ input: child.stdout }).on("line", (line) => {
			let record;
			try { record = JSON.parse(line); } catch { return; }
			if (record.type === "extension_error") result.errors.push(`${record.event}: ${record.error}`);
			if (record.type === "response" && record.success === false) result.errors.push(`${record.command}: ${record.error}`);
			if (record.type !== "extension_ui_request") return;
			if (["select", "confirm", "input", "editor"].includes(record.method)) send({ type: "extension_ui_response", id: record.id, cancelled: true });
			if (record.method !== "notify") return;
			if (!String(record.message).startsWith("AUDIT ")) return void result.errors.push(`notify: ${record.message}`);
			Object.assign(result, JSON.parse(record.message.slice(6)), { end: "ok" });
			clearTimeout(timer);
			done();
		});
	});
	await finished;
	child.kill("SIGKILL");
	// Messages that name the temporary home compare equal across runs.
	const home = (text) => text.replaceAll(dir, "<home>").replaceAll(`/private${dir}`, "<home>");
	result.errors = result.errors.map(home);
	result.stderr = home(result.stderr);
	await rm(dir, { recursive: true, force: true, maxRetries: 5 }).catch(() => undefined);
	return result;
}

/** What `run` differs in from `base`, the same side's run without a package. */
function own(result, base) {
	const minus = (list, baseList) => (list ?? []).filter((name) => !(baseList ?? []).includes(name));
	return { ...result, tools: minus(result.tools, base.tools), commands: minus(result.commands, base.commands) };
}

function differences(rust, upstream) {
	const diff = (kind, a, b) => [
		...b.filter((name) => !a.includes(name)).map((name) => `-${kind}:${name}`),
		...a.filter((name) => !b.includes(name)).map((name) => `+${kind}:${name}`),
	];
	const found = [...diff("tool", rust.tools, upstream.tools), ...diff("command", rust.commands, upstream.commands)];
	if (rust.end !== upstream.end) found.unshift(`end ${rust.end} (upstream ${upstream.end})`);
	found.push(...rust.errors.filter((error) => !upstream.errors.includes(error)).map((error) => `error ${error}`));
	const stderr = rust.stderr.split("\n").filter((line) => line.trim() && !upstream.stderr.includes(line));
	found.push(...stderr.map((line) => `stderr ${line}`));
	return found;
}

const [first, ...rest] = process.argv.slice(2);
if (first === "--top") {
	for (const spec of await top(Number(rest[0] ?? 70))) console.log(spec);
} else {
	const [upstreamCli, prefixes, ...specs] = rest;
	if (!first || !upstreamCli || !prefixes) {
		console.error("Usage: node scripts/compare-npm-extensions.mjs <pi> <upstream cli.js> <package-prefixes> [name@version ...]");
		console.error("       node scripts/compare-npm-extensions.mjs --top [count]");
		process.exit(2);
	}
	const list = specs.length ? specs : readFileSync(0, "utf8").split("\n").filter(Boolean);
	const sides = { rust: [resolve(first)], upstream: [process.execPath, resolve(upstreamCli)] };
	const base = { rust: await run(sides.rust), upstream: await run(sides.upstream) };
	let same = 0;
	for (const spec of list) {
		const path = packageDir(resolve(prefixes), spec);
		const [rust, upstream] = await Promise.all([run(sides.rust, path), run(sides.upstream, path)]);
		const found = differences(own(rust, base.rust), own(upstream, base.upstream));
		if (found.length === 0) {
			same += 1;
			continue;
		}
		console.log(`DIFF ${spec}`);
		for (const line of found) console.log(`  ${line.slice(0, 300)}`);
		process.exitCode = 1;
	}
	console.log(`${same} of ${list.length} packages behave as in upstream pi`);
}
