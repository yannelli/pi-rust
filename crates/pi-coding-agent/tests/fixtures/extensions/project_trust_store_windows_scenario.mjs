// Windows-only `ProjectTrustStore` cases: case folding, drive and shell path forms, ancestor bounds, and junctions.
// Runs on upstream under Node on Windows (expected output) and in the extension host. Paths read `<root>` and `<drive>`.
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const literal = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const letterCase = (path) => (/^[a-z]:/.test(path) ? "lower" : /^[A-Z]:/.test(path) ? "upper" : "none");

export async function scenario(c, options = {}) {
	if (process.platform !== "win32") throw new Error("the Windows trust store cases run only on win32");
	const root = realpathSync(mkdtempSync(join(options.tempParent ?? tmpdir(), "pi-trust-store-windows-")));
	const drive = root.slice(0, 2);
	const savedHome = ["HOME", "USERPROFILE"].map((name) => [name, name in process.env, process.env[name]]);
	// The root and its JSON-escaped form in any case, then its drive outside the root; the cases record letter case.
	const roots = [root, JSON.stringify(root).slice(1, -1)].map((text) => new RegExp(literal(text), "gi"));
	const drives = new RegExp(`${literal(drive)}(?=[\\\\/])`, "gi");
	const clean = (text) => roots.reduce((out, form) => out.replace(form, "<root>"), String(text)).replace(drives, "<drive>").replaceAll("\\", "/");
	const out = {};
	let agents = 0;
	const agentDir = () => join(root, `agent-${agents++}`);
	const trustFile = (dir) => join(dir, "trust.json");
	const bytes = (dir) => (existsSync(trustFile(dir)) ? clean(readFileSync(trustFile(dir), "utf8")) : null);
	const entry = (value) => (value === null ? null : { path: clean(value.path), driveLetter: letterCase(value.path), decision: value.decision });
	const fields = (error) => ({ message: clean(error?.message), code: error?.code ?? null });
	const attempt = (fn) => {
		try {
			return { value: fn() ?? null };
		} catch (error) {
			return { error: fields(error) };
		}
	};
	const written = (data) => {
		const dir = agentDir();
		mkdirSync(dir, { recursive: true });
		writeFileSync(trustFile(dir), `${JSON.stringify(data)}\n`);
		return dir;
	};
	try {
		const home = join(root, "home");
		mkdirSync(home, { recursive: true });
		process.env.HOME = home;
		process.env.USERPROFILE = home;
		const work = join(root, "Work");
		mkdirSync(join(work, "a", "b"), { recursive: true });

		let dir = agentDir();
		let store = new c.ProjectTrustStore(dir);
		store.set(work, true);
		out["a cwd that differs only in case finds the stored key"] = {
			upper: store.get(join(root, "WORK")),
			entry: entry(store.getEntry(join(root, "work", "a", "b"))),
			file: bytes(dir),
		};

		dir = written({ [join(root, "Work")]: false, [join(root, "work")]: true });
		store = new c.ProjectTrustStore(dir);
		const nullFirst = new c.ProjectTrustStore(written({ [join(root, "WORK")]: null, [join(root, "work")]: true }));
		out["two case variants: the first boolean in key order decides"] = {
			falseFirst: entry(store.getEntry(join(root, "WORK", "a"))),
			nullFirst: entry(nullFirst.getEntry(join(root, "WORK", "a"))),
		};
		store.setMany([{ path: join(root, "wOrK"), decision: true }]);
		out["setMany on a third casing replaces every variant"] = { file: bytes(dir) };
		store.set(join(root, "WORK"), null);
		out["set null on one casing removes them all"] = { value: store.get(work), file: bytes(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		store.set(`${drive.toLowerCase()}${join(work, "lower").slice(2)}`, true);
		store.set(`${join(work, "fwd").replaceAll("\\", "/")}/x/`, false);
		store.set(`${join(work, "a")}\\.\\b\\..\\dots\\`, true);
		out["drive letter case, forward slashes, and dot segments"] = {
			lower: entry(store.getEntry(join(work, "lower"))),
			slashes: entry(store.getEntry(join(work, "fwd", "x"))),
			dots: entry(store.getEntry(join(work, "a", "dots"))),
			file: bytes(dir),
		};

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		const shell = (prefix, leaf) => `${prefix}${drive[0].toLowerCase()}/${join(work, leaf).slice(3).replaceAll("\\", "/")}`;
		const shells = { gitBash: ["/", "shell-root"], wsl: ["/mnt/", "shell-mnt"], cygwin: ["/cygdrive/", "shell-cygdrive"] };
		const shellCase = {};
		for (const [name, [prefix, leaf]] of Object.entries(shells)) {
			store.set(shell(prefix, leaf), true);
			shellCase[name] = entry(store.getEntry(join(work, leaf)));
		}
		out["Git Bash, WSL, and Cygwin drive paths"] = { ...shellCase, file: bytes(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		store.set("~\\proj", true);
		out["~\\ expands to the home directory"] = { value: store.get(join(home, "proj")), file: bytes(dir) };

		dir = agentDir();
		store = new c.ProjectTrustStore(dir);
		store.setMany([
			{ path: `${drive}\\`, decision: true },
			{ path: join(root, "ab"), decision: false },
			{ path: join(root, "CASE"), decision: false },
		]);
		out["ancestor bounds: the drive root, a name prefix, and a case-folded ancestor"] = {
			deep: entry(store.getEntry(join(root, "x", "y"))),
			namePrefix: entry(store.getEntry(join(root, "abc"))),
			foldedAncestor: entry(store.getEntry(join(root, "case", "a", "b"))),
			file: bytes(dir),
		};

		for (const [name, type] of [["a junction is stored by its target", "junction"], ["a directory symlink is stored by its target", "dir"]]) {
			dir = agentDir();
			store = new c.ProjectTrustStore(dir);
			const target = join(root, `${type}-target`);
			const link = join(root, `${type}-link`);
			mkdirSync(target, { recursive: true });
			const made = attempt(() => symlinkSync(target, link, type));
			if (made.error) {
				out[name] = { skipped: `no ${type}`, error: made.error };
				continue;
			}
			store.set(link, true);
			out[name] = {
				viaLink: store.get(link),
				viaTarget: entry(store.getEntry(target)),
				again: attempt(() => symlinkSync(target, link, type)),
				file: bytes(dir),
			};
		}
	} finally {
		for (const [name, had, value] of savedHome) {
			if (had) process.env[name] = value;
			else delete process.env[name];
		}
		rmSync(root, { recursive: true, force: true });
	}
	return out;
}
