// Offline RPC checks for the package versions in docs/parity/extension-packages.md.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, realpath, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";

const packages = [
	["plan-mode", "0.60.3", "/plan start", "Plan mode enabled."],
	["goal", "0.55.3", "/goal status", "No goal is currently set"],
	["usage", "0.69.3", "/usage", "Provider usage"],
	["btw", "0.63.8", "/btw", "/btw requires interactive TUI mode"],
	["lsp", "0.49.12", "/lsp", "LSP command:"],
	["firecrawl", "0.51.1", "/firecrawl status", "Callable capability tools:"],
	["sync", "0.62.0", "/sync help", "Usage: /sync <command>"],
	["statusline", "0.51.1", "/statusline status", "pi-statusline source: built-in"],
	["starship", "0.67.3", "/starship status", "pi-starship source: built-in"],
	["codex-compact", "0.56.5", "/codex-compact", "Edit Responses compaction settings at"],
	["subagents", "4.0.1", "/subagents settings", "Edit user settings:"],
	["caffeinate", "0.49.15", "/caffeinate status", "disabled by PI_CAFFEINATE_DISABLED"],
	["accounts", "0.54.3", "/accounts", "No saved accounts yet"],
	["stamp", "0.54.2", "/stamp", "Presets"],
	["progress", "0.8.0", "/progress clear", "Progress widget cleared"],
	["herdr", "0.6.0", "/herdr", "Agent widget: On"],
	["notify", "0.4.4", "/notify", "Notify setup requires TUI"],
	["worktree", "0.51.10", "/worktree", "Git worktrees"],
	["codex-imagegen", "0.2.3", "/codex-imagegen", "/codex-imagegen requires TUI mode"],
	["ticker", "0.1.8", "/ticker help", "Usage: /ticker"],
	["chrome-devtools", "0.54.1", "/chrome-devtools status", "Chrome DevTools tools available:"],
	["notes", "0.2.6", "/notes", "/notes requires Pi TUI mode"],
	["tool", "0.4.3", "/tool", "Browse tools"],
	["chat", "0.1.13", "/chat", "Pi Chat requires Pi TUI mode"],
	["agentfs", "0.2.6"],
];

async function check(binary, packageRoot, dir, [name, version, message, expected], seededAccount = false) {
	const pkg = await realpath(join(packageRoot, "pi-" + name, "node_modules", "@narumitw", "pi-" + name));
	const manifest = JSON.parse(await readFile(join(pkg, "package.json"), "utf8"));
	assert.equal(manifest.name, "@narumitw/pi-" + name);
	assert.equal(manifest.version, version);
	const entries = await Promise.all(manifest.pi.extensions.map((entry) => realpath(join(pkg, entry))));
	const cwd = join(dir, "work");
	const agent = join(dir, "agent");
	await mkdir(cwd);
	await mkdir(agent);
	const env = {
		PATH: process.env.PATH, HOME: dir, USERPROFILE: dir, TMPDIR: dir, TMP: dir, TEMP: dir,
		XDG_CONFIG_HOME: dir, XDG_CACHE_HOME: dir, XDG_DATA_HOME: dir, PI_CODING_AGENT_DIR: agent,
		PI_OFFLINE: "1", PI_NODE_PATH: process.execPath, PI_CAFFEINATE_DISABLED: "1",
		GIT_CONFIG_NOSYSTEM: "1", TERM: "dumb",
	};
	if (process.platform === "win32") env.SystemRoot = process.env.SystemRoot;
	if (name === "herdr") Object.assign(env, { HERDR_ENV: "1", HERDR_PANE_ID: "smoke", HERDR_SOCKET_PATH: join(dir, "unused.sock") });
	if (name === "worktree") execFileSync("git", ["init", "-q"], { env, cwd, timeout: 5000 });
	const model = { baseUrl: "http://127.0.0.1:1/v1", api: "openai-completions", apiKey: "offline-fixture", models: [{ id: "offline" }] };
	await writeFile(join(agent, "models.json"), JSON.stringify({ providers: { audit: model } }));
	const native = name === "agentfs" || name === "chat";
	const options = ["--offline", "--no-approve", "--no-session", "--model", "audit/offline", "--mode", "rpc", "-e", pkg];
	if (native) options.push("--extension-runtime", "auto");
	const database = join(dir, "agentfs.db");
	if (name === "agentfs") options.push("--agentfs-path", database);
	if (seededAccount) {
		const providers = {
			openrouter: { accounts: { fixture: { type: "api_key", key: "fixture-openrouter" } } },
			"openai-codex": { accounts: { fixture: {
				type: "oauth", access: "fixture-openai-codex", refresh: "unused", expires: Date.now() + 86_400_000,
			} } },
		};
		await writeFile(join(agent, "pi-accounts.json"), JSON.stringify({ version: 1, providers }), { mode: 0o600 });
		const observer = join(dir, "observe.ts");
		await writeFile(observer, `export default function (pi) {
			pi.registerCommand("audit-account", { handler: async (_, ctx) => {
				const session = ctx.sessionManager;
				for (const provider of ["openrouter", "openai-codex"]) {
					const activate = (account) => new Promise(reply => pi.events.emit("accounts:activation:v1", {
						session, provider, account, reply
					}));
					const selected = await activate("fixture");
					const key = await ctx.modelRegistry.getApiKeyForProvider(provider);
					const saved = session.getEntries().some(e => e.type === "custom" && e.customType === "pi-accounts-selection"
						&& e.data.sessionId === session.getSessionId() && e.data.providers[provider] === "fixture");
					const overlay = ctx.modelRegistry.getRegisteredProviderConfig(provider);
					if (selected.status !== "active" || key !== "fixture-" + provider || !saved ||
						(provider === "openai-codex" && !overlay?.apiKey)) throw new Error(provider + " account activation failed");
					const cleared = await activate(null);
					const restored = await ctx.modelRegistry.getApiKeyForProvider(provider);
					if (cleared.status !== "inactive" || restored === key || ctx.modelRegistry.getRegisteredProviderConfig(provider))
						throw new Error(provider + " account restoration failed");
				}
				ctx.ui.notify("fixture accounts selected, recorded and cleared");
			} });
		}`);
		options.push("-e", observer);
	}

	const child = spawn(binary, options, { cwd, env, timeout: 90_000, killSignal: "SIGKILL" });
	const closed = once(child, "close");
	const input = createInterface({ input: child.stdout, crlfDelay: Infinity })[Symbol.asyncIterator]();
	let stderr = "";
	child.stderr.setEncoding("utf8").on("data", (text) => { stderr += text; });
	const send = (record) => child.stdin.write(JSON.stringify(record) + "\n");
	let requestId = 0;
	let activeCommand = false;
	let matched = false;
	let checkingShellGuard = false;
	const guardMessage = "AgentFS blocks ! and !! host commands; use the bash tool instead";
	function inspect(record) {
		const diagnostic = record.type === "extension_error" || record.notifyType === "error";
		const text = [record.title, record.message, record.error, ...(record.options ?? [])].filter(Boolean).join("\n");
		const expectedResult = activeCommand && expected && text.includes(expected);
		if (expectedResult) matched = true;
		const modeGuard = activeCommand && (
			(name === "btw" && record.method === "notify" && record.message === expected)
			|| (["notes", "chat"].includes(name) && record.event === "command"
				&& record.extensionPath === "command:" + name && record.error === expected + ".")
		);
		const expectedGuard = checkingShellGuard && record.type === "extension_error" && record.event === "user_bash"
			&& entries.includes(record.extensionPath) && record.error === guardMessage;
		if (diagnostic && !modeGuard && !expectedGuard) throw new Error(text);
		if (record.type === "extension_ui_request" && ["select", "confirm", "input", "editor"].includes(record.method)) {
			send({ type: "extension_ui_response", id: record.id, cancelled: true });
		}
	}
	async function request(type, args = {}) {
		const id = String(++requestId);
		send({ type, id, ...args });
		for (;;) {
			const { done, value } = await input.next();
			if (done) throw new Error(stderr || "pi closed RPC before its response");
			const record = JSON.parse(value);
			inspect(record);
			if (record.id === id && record.type === "response") {
				assert.ok(record.success, record.error);
				return record.data;
			}
		}
	}
	try {
		const { commands } = await request("get_commands");
		if (name === "agentfs") {
			assert.ok((await stat(database)).size > 0, "AgentFS did not initialize its database");
			checkingShellGuard = true;
			const result = await request("bash", { command: "echo audit", excludeFromContext: true });
			checkingShellGuard = false;
			assert.equal(result.exitCode, 1);
			assert.equal(result.output, guardMessage);
		} else {
			const command = commands.find((c) => c.name === message.slice(1).split(" ")[0] && c.source === "extension");
			assert.ok(command && entries.includes(await realpath(command.sourceInfo.path)), "package did not register its command");
			if (seededAccount) {
				message = "/audit-account";
				expected = "fixture accounts selected, recorded and cleared";
			}
			activeCommand = true;
			const result = await request("prompt", { message });
			activeCommand = false;
			assert.equal(result.agentInvoked, false);
			assert.ok(matched, "missing command output: " + expected);
		}
		child.stdin.end();
		for (let line = await input.next(); !line.done; line = await input.next()) inspect(JSON.parse(line.value));
		const [code] = await closed;
		assert.equal(code, 0, stderr);
		assert.doesNotMatch(stderr, /Using QuickJS|Failed to load extension/i);
	} finally {
		if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
		await closed;
	}
}

/** `path`, or `path.exe` on Windows when the suffix was left out. */
async function executable(path) {
	try {
		return await realpath(path);
	} catch (error) {
		if (process.platform !== "win32" || error.code !== "ENOENT") throw error;
		return realpath(path + ".exe");
	}
}

const [binary, packageRoot, only] = process.argv.slice(2);
if (binary === "--list") {
	for (const [name, version] of packages) console.log(`@narumitw/pi-${name}@${version}`);
} else {
	assert.ok(binary && packageRoot, "Usage: node scripts/check-upstream-extensions.mjs <pi> <package-prefixes> [pi-name]");
	const cases = packages.filter(([name]) => !only || only === "pi-" + name);
	assert.ok(cases.length, "unknown package: " + only);
	for (const entry of cases) {
		const dir = await mkdtemp(join(tmpdir(), "pi-ext-audit-"));
		try {
			await check(await executable(binary), resolve(packageRoot), dir, entry);
			if (entry[0] === "accounts") {
				const seededDir = join(dir, "seeded");
				await mkdir(seededDir);
				await check(await executable(binary), resolve(packageRoot), seededDir, entry, true);
			}
			console.log("PASS pi-" + entry[0] + "@" + entry[1]);
		} catch (error) {
			console.error("FAIL pi-" + entry[0] + ": " + error.message);
			process.exitCode = 1;
		} finally {
			await rm(dir, { recursive: true, force: true });
		}
	}
}
