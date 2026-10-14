// The documentation audit's structured verdict tool, loaded with `-e` by the host eval.
export default function (pi) {
	pi.registerTool({
		name: "submit_documentation_audit",
		label: "Submit documentation audit",
		description: "Submit the final verdict after completing the documentation investigation.",
		promptSnippet: "Submit the final documentation audit as validated structured data",
		parameters: {
			type: "object",
			properties: {
				verdict: { anyOf: [{ const: "match" }, { const: "mismatch" }, { const: "inconclusive" }] },
				explanation: { type: "string", minLength: 1, maxLength: 2000 },
			},
			required: ["verdict", "explanation"],
			additionalProperties: false,
		},
		constrainedSampling: { type: "json_schema", strict: "prefer" },
		async execute(_toolCallId, params) {
			return {
				content: [{ type: "text", text: "Documentation audit submitted." }],
				details: params,
				terminate: true,
			};
		},
	});
}
