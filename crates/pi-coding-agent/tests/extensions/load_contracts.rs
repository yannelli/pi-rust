//! Contracts that Pi 1.6 extensions use while they load (issue #250): built-in tool factories,
//! TypeBox default imports, the `File` global, `AgentSession.prototype`, and provider objects.

use serde_json::{Value, json};

use crate::extensions::harness::{Setup, harness};

const PROBE: &str = r#"import Value from "typebox/value";
import Type from "typebox";
import Compile from "typebox/compile";
import { AgentSession, createReadTool } from "@earendil-works/pi-coding-agent";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

const outcome = (run) => {
	try {
		return run();
	} catch (error) {
		return { code: error.code ?? null, message: error.message };
	}
};
const noStream = () => {
	throw new Error("The probe never streams");
};

export default function (pi) {
	const atLoad = outcome(() => typeof createReadTool(process.cwd()).execute);
	pi.registerCommand("contracts", {
		handler: async (_args, ctx) => {
			writeFileSync(join(ctx.cwd, "probe.txt"), "hello from the probe\n");
			const read = await createReadTool(ctx.cwd).execute("call-1", { path: "probe.txt" });
			pi.registerProvider({ id: "object-provider", name: "Object", auth: {}, getModels: () => [], stream: noStream, streamSimple: noStream });
			pi.registerProvider({ name: "No id", auth: {}, getModels: () => [], stream: noStream, streamSimple: noStream });
			pi.appendEntry("contracts", {
				atLoad,
				read: read.content.map((block) => block.text).join(""),
				typebox: [typeof Value.Check, typeof Type.Object, typeof Compile],
				file: new File(["x"], "probe.txt").size,
				compact: typeof AgentSession.prototype.compact,
				compactCall: outcome(() => AgentSession.prototype.compact.call({})),
				construct: outcome(() => new AgentSession()),
			});
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn pi_1_6_load_contracts_hold() {
    let h =
        harness(Setup { files: vec![("probe.ts", PROBE.into())], load: vec!["probe.ts"], ..Setup::default() }).await;
    assert_eq!(h.errors(), Vec::<String>::new());
    h.session.prompt("/contracts", Vec::new()).await.unwrap();
    let entries = h.entries();
    let data = entries.iter().find(|entry| entry["customType"] == "contracts").map(|entry| &entry["data"]);
    let data = data.cloned().unwrap_or(Value::Null);
    assert_eq!(data["atLoad"], "function", "{data}");
    assert!(data["read"].as_str().is_some_and(|text| text.contains("hello from the probe")), "{data}");
    assert_eq!(data["typebox"], json!(["function", "function", "function"]), "{data}");
    assert_eq!(data["file"], 1, "{data}");
    assert_eq!(data["compact"], "function", "{data}");
    let unsupported = |member: &str| json!({"code": "ERR_PI_UNSUPPORTED", "message": format!("{member} is not supported in pi-rust")});
    assert_eq!(data["compactCall"], unsupported("AgentSession.compact"), "{data}");
    assert_eq!(data["construct"], unsupported("AgentSession"), "{data}");
    assert_eq!(
        h.errors(),
        [
            "register_provider: Provider object-provider: registering a provider object is not supported in pi-rust. \
             Use pi.registerProvider(\"object-provider\", { api, baseUrl, models, streamSimple }).",
            "register_provider: Provider id must not be empty.",
        ]
    );
    assert!(h.session.model_runtime().get_model("object-provider", "any").is_none());
}
