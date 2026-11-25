//! Contracts that Pi 1.6 extensions use while they load (issue #250): built-in tool factories,
//! TypeBox default imports, the `File` global, `AgentSession.prototype`, and provider objects.

use std::process::Stdio;
use std::time::Duration;

use pi_coding_agent::rpc::jsonl::JsonlReader;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::extensions::harness::{Setup, harness};

/// An upstream `Provider` object, a provider config passed as one object, and a `Provider` without
/// an id, for `pi.registerProvider`. Each config model streams through its own API.
const PROVIDERS: &str = r#"const noStream = () => {
	throw new Error("The probe never streams");
};
const nativeProvider = (id) => ({ id, name: "Native", auth: {}, getModels: () => [], stream: noStream, streamSimple: noStream });
const model = { id: "config-model", name: "Config model", reasoning: false, input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 4096, maxTokens: 128 };
const configProvider = (api) => ({ id: "config-object", api, baseUrl: "http://127.0.0.1:9/v1", apiKey: "not-a-secret",
	models: [model], streamSimple: noStream });
"#;

fn probe() -> String {
    format!(
        r#"import Value from "typebox/value";
import Type from "typebox";
import Compile from "typebox/compile";
import {{ AgentSession, createReadTool }} from "@earendil-works/pi-coding-agent";
import {{ writeFileSync }} from "node:fs";
import {{ join }} from "node:path";
{PROVIDERS}
const outcome = (run) => {{
	try {{
		return run();
	}} catch (error) {{
		return {{ code: error.code ?? null, message: error.message }};
	}}
}};

export default function (pi) {{
	const atLoad = outcome(() => typeof createReadTool(process.cwd()).execute);
	pi.registerCommand("contracts", {{
		handler: async (_args, ctx) => {{
			writeFileSync(join(ctx.cwd, "probe.txt"), "hello from the probe\n");
			const read = await createReadTool(ctx.cwd).execute("call-1", {{ path: "probe.txt" }});
			pi.registerProvider(nativeProvider("object-provider"));
			pi.registerProvider(nativeProvider(undefined));
			pi.registerProvider(configProvider("load-contracts-bound"));
			pi.appendEntry("contracts", {{
				atLoad,
				read: read.content.map((block) => block.text).join(""),
				typebox: [typeof Value.Check, typeof Type.Object, typeof Compile],
				file: new File(["x"], "probe.txt").size,
				compact: typeof AgentSession.prototype.compact,
				compactCall: outcome(() => AgentSession.prototype.compact.call({{}})),
				construct: outcome(() => new AgentSession()),
				providers: ctx.modelRegistry.getRegisteredProviderIds(),
			}});
		}},
	}});
}}
"#
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_1_6_load_contracts_hold() {
    let h = harness(Setup { files: vec![("probe.ts", probe())], load: vec!["probe.ts"], ..Setup::default() }).await;
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
    assert_eq!(data["providers"], json!(["object-provider", "config-object"]), "{data}");
    assert_eq!(h.errors(), ["register_provider: Provider id must not be empty."]);
    assert!(h.session.model_runtime().is_native_provider("object-provider"));
    assert!(h.session.model_runtime().get_model("config-object", "config-model").is_some());
}

/// The path of #250: the factory registers before models bind; both forms register.
#[tokio::test(flavor = "multi_thread")]
async fn provider_objects_from_the_factory_register_at_startup() {
    let root = tempfile::tempdir().unwrap();
    crate::cli_support::setup(root.path(), "http://127.0.0.1:9/v1");
    let extension = root.path().join("providers.ts");
    let source = format!(
        r#"{PROVIDERS}
export default function (pi) {{
	pi.registerProvider(nativeProvider("object-provider"));
	pi.registerProvider(configProvider("load-contracts-startup"));
	pi.registerCommand("ids", {{
		handler: async (_args, ctx) => ctx.ui.notify(`ids ${{ctx.modelRegistry.getRegisteredProviderIds().join(",")}}`),
	}});
}}
"#
    );
    std::fs::write(&extension, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    crate::cli_support::isolate(command.as_std_mut(), root.path());
    let args = ["--mode", "rpc", "--no-session", "--model", "mock-model", "-e", &extension.display().to_string()];
    command.args(args).current_dir(root.path().join("work")).kill_on_drop(true);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = JsonlReader::new(child.stdout.take().unwrap());
    let requests = [
        json!({"id": "model", "type": "set_model", "provider": "config-object", "modelId": "config-model"}),
        json!({"id": "ids", "type": "prompt", "message": "/ids"}),
    ];
    for request in requests {
        stdin.write_all(format!("{request}\n").as_bytes()).await.unwrap();
    }
    stdin.flush().await.unwrap();
    let records = async {
        let mut records = Vec::new();
        while let Some(line) = stdout.next_line().await.unwrap() {
            let record: Value = serde_json::from_str(&line).unwrap();
            let done = record["method"] == "notify";
            records.push(record);
            if done {
                return records;
            }
        }
        records
    };
    let records = tokio::time::timeout(Duration::from_secs(30), records).await.expect("notify in time");
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await.unwrap().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let model = records.iter().find(|record| record["id"] == "model").cloned().unwrap_or(Value::Null);
    assert_eq!(model["success"], true, "{model} {stderr}");
    let ids = Some(json!("ids config-object,object-provider"));
    assert_eq!(records.last().map(|record| record["message"].clone()), ids, "{stderr}");
    let errors: Vec<&str> = stderr.lines().filter(|line| line.starts_with("Extension error (")).collect();
    assert_eq!(errors, Vec::<&str>::new(), "{stderr}");
}
