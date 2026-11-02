// The child-process cases, each in a process of its own: a caller getter exits, or leaves the outer lock unowned,
// while the store holds its lock. The report is written inside the child; marker files show code after an exit.
import { existsSync, rmdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export const MODES = ["exit", "reallyExit", "compromised"];

const fields = (error) => ({ code: error?.code ?? null, message: String(error?.message) });

export function run(c, { agentDir, reportPath, mode, code = 7, waitMs = 20000 }) {
	const store = new c.ProjectTrustStore(agentDir);
	const lock = join(agentDir, "trust.json.lock");
	const events = [];
	const write = () => writeFileSync(reportPath, JSON.stringify(events));
	if (mode === "compromised") return compromised(store, lock, events, write, { agentDir, waitMs });
	process.on("exit", (exitCode) => {
		events.push({ listener: "exit", exitCode, lockHeld: existsSync(lock) });
		const started = Date.now();
		try {
			events.push({ nested: "ok", value: store.get(agentDir) });
		} catch (error) {
			events.push({ nested: fields(error), waitedAtLeast150ms: Date.now() - started >= 150 });
		}
		write();
	});
	try {
		store.setMany([{ get path() {
			events.push({ getter: mode, lockHeld: existsSync(lock) });
			if (mode === "reallyExit") {
				write();
				process.reallyExit(code);
			}
			process.exit(code);
		}, decision: true }]);
		writeFileSync(`${reportPath}.returned`, "");
	} finally {
		writeFileSync(`${reportPath}.finally`, "");
	}
}

// A getter removes the lock dir and calls the store again: the inner call takes over the held-lock entry, so the
// outer release fails and leaves the outer update timer, whose error arrives later as an uncaught exception.
function compromised(store, lock, events, write, { agentDir, waitMs }) {
	return new Promise((resolve) => {
		let released = null;
		const finish = () => {
			clearTimeout(bound);
			process.removeListener("uncaughtException", onError);
			write();
			resolve();
		};
		const onError = (error) => {
			const waited = released === null ? null : Date.now() - released;
			events.push({ uncaught: fields(error), afterAtLeast4000ms: waited !== null && waited >= 4000, lockExists: existsSync(lock) });
			finish();
		};
		const bound = setTimeout(() => {
			events.push({ uncaught: null, timedOutAfterMs: waitMs });
			finish();
		}, waitMs);
		process.on("uncaughtException", onError);
		try {
			store.setMany([{ get path() {
				rmdirSync(lock);
				try {
					events.push({ inner: "ok", value: store.get(agentDir) });
				} catch (error) {
					events.push({ inner: fields(error) });
				}
				return agentDir;
			}, decision: true }]);
			events.push({ outer: "ok" });
		} catch (error) {
			events.push({ outer: fields(error), lockExists: existsSync(lock) });
		}
		released = Date.now();
	});
}
