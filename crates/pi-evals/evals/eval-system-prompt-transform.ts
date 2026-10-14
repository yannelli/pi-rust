// Hidden eval extension for the without_docs variant: sends the default prompt without
// its Pi documentation section. Mirrors `exclude_pi_documentation` in src/harness.rs.
export default function (pi) {
	pi.on("before_agent_start", (event) => {
		const prompt = event.systemPrompt;
		const startMarker = "\n<docs>\n";
		const endMarker = "\n</docs>";
		const start = prompt.indexOf(startMarker);
		if (start === -1) throw new Error("Default Pi system prompt has no Pi documentation section.");
		const end = prompt.indexOf(endMarker, start);
		if (end === -1) throw new Error("Default Pi system prompt has no complete Pi documentation section.");
		if (prompt.lastIndexOf("\n<cwd>\n") < end) {
			throw new Error("Default Pi system prompt has no working-directory section.");
		}
		return { systemPrompt: prompt.slice(0, start) + prompt.slice(end + endMarker.length) };
	});
}
