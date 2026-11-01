// Runs `loadProjectContextFiles` over temporary trees, on upstream under Node (expected output) and in the extension
// host. Paths read `<root>/...` with `/` on every platform; warnings drop chalk's yellow wrapper (see color cases).
import { chmodSync, mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";

export async function scenario(c, options = {}) {
	const root = realpathSync(mkdtempSync(join(options.tempParent ?? tmpdir(), "pi-context-scenario-")));
	const savedHome = ["HOME", "USERPROFILE"].map((name) => [name, name in process.env, process.env[name]]);
	const savedError = console.error;
	const locked = [];
	const out = {};
	let trees = 0;
	const dir = (...parts) => {
		const path = join(...parts);
		mkdirSync(path, { recursive: true });
		return path;
	};
	const file = (path, content) => {
		dir(join(path, ".."));
		writeFileSync(path, content);
		return path;
	};
	const tree = () => dir(root, `tree-${trees++}`);
	const clean = (text) => text.split(root).join("<root>").replaceAll("\\", "/");
	const unwrap = (text) => (text.startsWith("\u001B[33m") && text.endsWith("\u001B[39m") ? text.slice(5, -5) : text);
	const check = (name, cwd, agentDir) => {
		const warnings = [];
		console.error = (...args) => warnings.push(clean(unwrap(args.map(String).join(" "))));
		try {
			const files = c.loadProjectContextFiles({ cwd, agentDir });
			const inside = files.filter((entry) => entry.path.startsWith(root));
			out[name] = {
				files: inside.map((entry) => ({ path: clean(entry.path), content: entry.content })),
				outside: files.length - inside.length,
				warnings,
			};
		} catch (error) {
			out[name] = { error: clean(error instanceof Error ? error.message : String(error)), warnings };
		} finally {
			console.error = savedError;
		}
	};
	const gitDir = (path, head = true) => {
		dir(path);
		if (head) file(join(path, "HEAD"), "ref: refs/heads/main\n");
		return path;
	};
	const worktree = (main, name, at) => {
		const admin = gitDir(join(main, ".git", "worktrees", name));
		file(join(admin, "commondir"), "../..\n");
		file(join(at, ".git"), `gitdir: ${admin}\n`);
		return at;
	};
	try {
		const home = dir(root, "home");
		process.env.HOME = home;
		if (process.platform === "win32") process.env.USERPROFILE = home;
		const agent = dir(root, "agent");

		check("no files", tree(), dir(root, "empty-agent"));

		const ordered = tree();
		file(join(agent, "AGENTS.md"), "global");
		file(join(ordered, "AGENTS.md"), "outer");
		file(join(ordered, "mid", "CLAUDE.md"), "middle");
		file(join(ordered, "mid", "inner", "AGENTS.md"), "inner");
		check("agent dir, then outermost to cwd", join(ordered, "mid", "inner"), agent);

		const override = tree();
		file(join(override, "AGENTS.override.md"), "override");
		file(join(override, "AGENTS.md"), "plain");
		check("AGENTS.override.md wins", override, dir(root, "empty-agent"));
		const dirNamed = tree();
		dir(dirNamed, "AGENTS.md");
		file(join(dirNamed, "CLAUDE.md"), "claude");
		check("a directory named AGENTS.md falls through to CLAUDE.md", dirNamed, dir(root, "empty-agent"));
		const broken = tree();
		symlinkSync(join(root, "missing"), join(broken, "AGENTS.md"));
		file(join(broken, "CLAUDE.md"), "after broken link");
		check("a broken AGENTS.md symlink is skipped without a warning", broken, dir(root, "empty-agent"));

		const bom = tree();
		file(join(bom, "AGENTS.md"), "﻿﻿two boms");
		check("one BOM is removed", bom, dir(root, "empty-agent"));
		const invalid = tree();
		file(join(invalid, "AGENTS.md"), Buffer.from([0x68, 0xff, 0x69]));
		check("invalid UTF-8 decodes to U+FFFD", invalid, dir(root, "empty-agent"));

		const unreadable = tree();
		file(join(unreadable, "AGENTS.md"), "secret");
		file(join(unreadable, "CLAUDE.md"), "fallback");
		const canLock = process.platform !== "win32" && process.getuid?.() !== 0;
		if (canLock) {
			chmodSync(join(unreadable, "AGENTS.md"), 0o000);
			locked.push(join(unreadable, "AGENTS.md"));
			check("an unreadable AGENTS.md warns and falls through", unreadable, dir(root, "empty-agent"));
		} else {
			out["an unreadable AGENTS.md warns and falls through"] = "skipped: no permission bits";
		}

		const same = tree();
		file(join(same, "AGENTS.md"), "same");
		check("agentDir equal to cwd gives one entry", same, same);
		const ancestorAgent = tree();
		file(join(ancestorAgent, "AGENTS.md"), "ancestor agent");
		check("agentDir that is an ancestor gives one entry", dir(ancestorAgent, "a", "b"), ancestorAgent);

		const main = tree();
		gitDir(join(main, ".git"));
		file(join(main, "AGENTS.md"), "main");
		const nested = worktree(main, "wt", dir(main, "wt"));
		file(join(nested, "AGENTS.md"), "worktree");
		check("a nested linked worktree shadows the main checkout", nested, dir(root, "empty-agent"));
		symlinkSync(nested, join(root, "linked-worktree"));
		check("a symlinked worktree cwd", join(root, "linked-worktree"), dir(root, "empty-agent"));
		const plainChild = dir(main, "plain");
		file(join(plainChild, "AGENTS.md"), "plain child");
		check("an ordinary subdirectory inherits the main file", plainChild, dir(root, "empty-agent"));
		const noOwn = worktree(main, "empty", dir(main, "empty"));
		check("a nested worktree without its own file inherits", noOwn, dir(root, "empty-agent"));
		const linkedMain = tree();
		gitDir(join(linkedMain, ".git"));
		file(join(linkedMain, "AGENTS.md"), "main via link");
		const linkedWt = worktree(linkedMain, "wt", dir(linkedMain, "wt"));
		symlinkSync(join(linkedMain, "AGENTS.md"), join(linkedWt, "AGENTS.md"));
		check("a worktree AGENTS.md that links to the main file", linkedWt, dir(root, "empty-agent"));

		const siblingMain = tree();
		gitDir(join(siblingMain, ".git"));
		file(join(siblingMain, "AGENTS.md"), "sibling main");
		const sibling = worktree(siblingMain, "side", dir(root, `tree-${trees++}`));
		file(join(sibling, "AGENTS.md"), "sibling");
		check("a sibling worktree", sibling, dir(root, "empty-agent"));
		const bare = tree();
		gitDir(join(bare, ".bare"));
		file(join(bare, "AGENTS.md"), "bare parent");
		const admin = gitDir(join(bare, ".bare", "worktrees", "main"));
		file(join(admin, "commondir"), "../..\n");
		file(join(dir(bare, "main"), ".git"), `gitdir: ${admin}\n`);
		file(join(bare, "main", "AGENTS.md"), "bare main");
		check("a bare layout does not shadow", join(bare, "main"), dir(root, "empty-agent"));
		const superRepo = tree();
		gitDir(join(superRepo, ".git"));
		file(join(superRepo, "AGENTS.md"), "super");
		gitDir(join(superRepo, ".git", "modules", "sub"));
		file(join(dir(superRepo, "sub"), ".git"), "gitdir: ../.git/modules/sub\n");
		file(join(superRepo, "sub", "AGENTS.md"), "submodule");
		check("a submodule does not shadow", join(superRepo, "sub"), dir(root, "empty-agent"));
		const junk = dir(main, "junk");
		file(join(junk, ".git"), "not a gitdir line\n");
		file(join(junk, "AGENTS.md"), "junk");
		check("a .git file without gitdir: continues to the parent", junk, dir(root, "empty-agent"));
		const headless = tree();
		gitDir(join(headless, ".git"));
		const inner = worktree(headless, "wt", dir(headless, "wt"));
		gitDir(join(inner, "deeper", ".git"), false);
		file(join(headless, "AGENTS.md"), "headless main");
		file(join(inner, "AGENTS.md"), "headless worktree");
		check("a .git directory without HEAD ends the search", join(inner, "deeper"), dir(root, "empty-agent"));

		const relativeTree = tree();
		file(join(relativeTree, "AGENTS.md"), "relative");
		check("relative cwd and agentDir", relative(process.cwd(), relativeTree), relative(process.cwd(), agent));
		file(join(home, "proj", "AGENTS.md"), "tilde project");
		file(join(home, "agent", "AGENTS.md"), "tilde agent");
		check("~ cwd and agentDir", "~/proj", "~/agent");
	} finally {
		console.error = savedError;
		for (const path of locked) chmodSync(path, 0o600);
		for (const [name, had, value] of savedHome) {
			if (had) process.env[name] = value;
			else delete process.env[name];
		}
		rmSync(root, { recursive: true, force: true });
	}
	return out;
}
