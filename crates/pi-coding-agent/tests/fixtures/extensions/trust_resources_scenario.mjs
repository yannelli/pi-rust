// Runs `hasTrustRequiringProjectResources` over temporary project trees, on upstream under Node
// (expected output) and in the extension host. Only booleans are recorded, so paths never differ.
import { mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";

const ENTRIES = ["settings.json", "mcp.json", "extensions", "skills", "prompts", "themes", "SYSTEM.md", "APPEND_SYSTEM.md"];

export async function scenario(c) {
	const root = realpathSync(mkdtempSync(join(tmpdir(), "pi-trust-scenario-")));
	const saved = ["HOME", "USERPROFILE"].map((name) => [name, name in process.env, process.env[name]]);
	const platform = Object.getOwnPropertyDescriptor(process, "platform");
	// `os.homedir()` reads USERPROFILE on Windows and HOME elsewhere; both point at the temporary home.
	const setHome = (path) => {
		process.env.HOME = path;
		if (process.platform === "win32") process.env.USERPROFILE = path;
	};
	const link = (target, path) => symlinkSync(target, path, "junction");
	const out = {};
	let projects = 0;
	const dir = (...parts) => {
		const path = join(...parts);
		mkdirSync(path, { recursive: true });
		return path;
	};
	const file = (...parts) => {
		dir(join(...parts, ".."));
		writeFileSync(join(...parts), "x");
	};
	const project = () => dir(root, `project-${projects++}`);
	const check = (name, cwd) => {
		try {
			out[name] = c.hasTrustRequiringProjectResources(cwd);
		} catch (error) {
			out[name] = `error: ${error instanceof Error ? error.message : String(error)}`;
		}
	};
	try {
		const home = dir(root, "home");
		setHome(home);

		check("empty", project());
		const bare = project();
		dir(bare, ".pi");
		check("bare .pi", bare);
		const unrelated = project();
		file(unrelated, ".pi", "other.txt");
		check(".pi/other.txt", unrelated);
		for (const entry of ENTRIES) {
			const asFile = project();
			file(asFile, ".pi", entry);
			check(`file .pi/${entry}`, asFile);
			const asDir = project();
			dir(asDir, ".pi", entry);
			check(`dir .pi/${entry}`, asDir);
		}

		const broken = project();
		dir(broken, ".pi");
		link(join(root, "missing-target"), join(broken, ".pi", "settings.json"));
		check("broken symlink .pi/settings.json", broken);
		const linkedEntry = project();
		dir(linkedEntry, ".pi");
		link(dir(root, "real-extensions"), join(linkedEntry, ".pi", "extensions"));
		check("symlinked .pi/extensions", linkedEntry);
		const trusted = project();
		dir(trusted, ".pi", "skills");
		link(trusted, join(root, "linked-trusted"));
		check("symlinked cwd with .pi/skills", join(root, "linked-trusted"));
		link(project(), join(root, "linked-plain"));
		check("symlinked cwd without resources", join(root, "linked-plain"));

		const skills = project();
		dir(skills, ".agents", "skills");
		check(".agents/skills in cwd", skills);
		const ancestor = project();
		dir(ancestor, ".agents", "skills");
		check(".agents/skills in an ancestor", dir(ancestor, "a", "b"));
		const skillsFile = project();
		file(skillsFile, ".agents", "skills");
		check(".agents/skills as a file", skillsFile);

		dir(home, ".agents", "skills");
		check("HOME with only HOME/.agents/skills", home);
		check("under HOME with only HOME/.agents/skills", dir(home, "work"));
		file(home, "configured", ".pi", "settings.json");
		check("under HOME with .pi/settings.json", join(home, "configured"));
		const realHome = dir(root, "real-home");
		dir(realHome, ".agents", "skills");
		link(realHome, join(root, "home-link"));
		setHome(join(root, "home-link"));
		check("symlinked HOME, cwd under its real path", dir(realHome, "work"));
		check("symlinked HOME, cwd under the link", join(root, "home-link", "work"));

		setHome(home);
		dir(home, "tilde", ".pi", "prompts");
		check("~/tilde with .pi/prompts", "~/tilde");
		check("~/missing under HOME", "~/missing");
		const relativeTrusted = project();
		dir(relativeTrusted, ".pi", "themes");
		check("relative cwd with .pi/themes", relative(process.cwd(), relativeTrusted));
		check("relative cwd without resources", relative(process.cwd(), project()));

		// Windows path rules compare case-insensitively, so HOME in other letters still names the user's skills.
		Object.defineProperty(process, "platform", { value: "win32", configurable: true });
		setHome(home.toUpperCase());
		check("win32 rules, HOME in other letter case", home);
		Object.defineProperty(process, "platform", platform);
		setHome(home);

		delete process.env.HOME;
		check("HOME unset, .agents/skills in cwd", skills);
		check("HOME unset, no resources", project());
		process.env.HOME = "";
		check("HOME empty, .agents/skills in cwd", skills);
		check("HOME empty, no resources", project());
	} finally {
		Object.defineProperty(process, "platform", platform);
		for (const [name, had, value] of saved) {
			if (had) process.env[name] = value;
			else delete process.env[name];
		}
		rmSync(root, { recursive: true, force: true });
	}
	return out;
}
