/**
 * Notes extension: a test fixture shaped like the upstream examples (hello.ts, claude-rules.ts).
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { Type } from "typebox";
import { defineTool, type ExtensionAPI, type ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Text } from "@mariozechner/pi-tui";
import slugify from "slug-lite";
import { nextCount } from "./lib/counter";

enum Level {
	Info = "info",
	Warning = "warning",
}

interface NoteDetails {
	file: string;
	bytes: number;
}

const notesDir = (ctx: ExtensionContext): string => path.join(ctx.cwd, ".notes");

const writeNote = defineTool({
	name: "write_note",
	label: "Write note",
	description: "Write a markdown note under .notes",
	parameters: Type.Object({
		title: Type.String({ description: "Note title" }),
		body: Type.String(),
	}),
	async execute(_toolCallId, params, _signal, onUpdate, ctx) {
		onUpdate?.({ content: [{ type: "text", text: `writing ${params.title}` }] });
		await fs.promises.mkdir(notesDir(ctx), { recursive: true });
		const file = path.join(notesDir(ctx), `${slugify(params.title)}.md`);
		await fs.promises.writeFile(file, `# ${params.title}\n\n${params.body}\n`);
		const details: NoteDetails = { file: path.relative(ctx.cwd, file), bytes: fs.statSync(file).size };
		return { content: [{ type: "text", text: `Saved ${details.file}` }], details };
	},
});

export default function notesExtension(pi: ExtensionAPI) {
	let sessions = 0;

	pi.registerTool(writeNote);

	pi.registerCommand("notes", {
		description: "List saved notes",
		handler: async (_args: string, ctx: ExtensionContext) => {
			const dir = notesDir(ctx);
			const names = fs.existsSync(dir) ? fs.readdirSync(dir).sort() : [];
			ctx.ui.notify(names.length > 0 ? names.join(", ") : "no notes", Level.Info);
		},
	});

	pi.on("session_start", async (_event, ctx) => {
		sessions = nextCount(sessions);
		ctx.ui.notify(`session ${sessions} in ${path.basename(ctx.cwd)}`, Level.Info);
	});

	pi.on("tool_call", (event) => {
		if (event.toolName === "bash" && String(event.input.command).includes("rm -rf")) {
			return { block: true, reason: "destructive command" };
		}
		return undefined;
	});

	pi.registerMessageRenderer("note", (message, _options, theme) => new Text(theme.fg("accent", String(message.content)), 0, 0));
}
