// Read a JSON array of Markdown strings on stdin and print the top-level marked code tokens.
// Usage: node marked-code-blocks.mjs [marked.min.js] < sources.json
import { createHash } from "node:crypto";
import fs from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const PINNED_SHA256 = "438eedfcf932a414d0d0bfeea32dc365c063563b8ca713b4687fc8f8b501e5e4";
const here = path.dirname(fileURLToPath(import.meta.url));
const vendored = path.join(here, "../../pi-coding-agent/src/export/assets/vendor/marked.min.js");
const bundle = path.resolve(process.argv[2] ?? vendored);
const hash = createHash("sha256").update(fs.readFileSync(bundle)).digest("hex");
if (!process.argv[2] && hash !== PINNED_SHA256) throw new Error(`${bundle} is not the pinned marked 18.0.11 (sha256 ${hash})`);
const { Marked } = createRequire(import.meta.url)(bundle);
const sources = JSON.parse(fs.readFileSync(0, "utf8"));
const tokens = sources.map((source) =>
	new Marked()
		.lexer(source)
		.filter((token) => token.type === "code")
		.map((token) => [token.raw, token.lang ?? "", token.text]),
);
process.stdout.write(JSON.stringify(tokens));
