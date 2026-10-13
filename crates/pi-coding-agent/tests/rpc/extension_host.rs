//! RPC mode with the extension host: JavaScript extensions list their commands, ask the client
//! through `extension_ui_request` records, and see prompts with source `rpc`.

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extensions::rpc::ExtensionRpcHost;
use pi_coding_agent::rpc::RpcModeOptions;
use serde_json::{Value, json};

use crate::harness::Options;
use crate::support::{Rpc, start_with};

const EXTENSION: &str = r#"export default function (pi) {
	pi.on("input", (event, ctx) => { ctx.ui.notify(`source ${event.source}`); });
	pi.registerCommand("greet", {
		description: "Greet someone",
		handler: async (_args, ctx) => {
			const who = await ctx.ui.select("Who?", ["Ada", "Grace"]);
			ctx.ui.notify(`hello ${who}`, "info");
			ctx.ui.setStatus("greet", `greeted ${who}`);
		},
	});
}
"#;

async fn notified(rpc: &Rpc, message: &str) -> Value {
    rpc.wait_for(|record| {
        record["type"] == "extension_ui_request" && record["method"] == "notify" && record["message"] == message
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn extensions_run_with_the_client_as_their_ui() {
    let options = Options { responses: vec![FauxResponse::text("hi there")], ..Options::default() };
    let rpc_options = RpcModeOptions { extension_host: Some(ExtensionRpcHost::shared()), handle_signals: false };
    let mut rpc = start_with(options, rpc_options, |h, _| {
        std::fs::create_dir_all(h.agent_dir.join("extensions")).unwrap();
        std::fs::write(h.agent_dir.join("extensions/greet.ts"), EXTENSION).unwrap();
    })
    .await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    let greet = commands.as_array().unwrap().iter().find(|command| command["name"] == "greet").cloned();
    let greet = greet.expect("extension command listed");
    assert_eq!((greet["source"].as_str(), greet["description"].as_str()), (Some("extension"), Some("Greet someone")));

    rpc.send(json!({"id": "g", "type": "prompt", "message": "/greet"})).await;
    let select = rpc.wait_for(|record| record["method"] == "select").await;
    assert_eq!((select["title"].as_str(), select["options"].clone()), (Some("Who?"), json!(["Ada", "Grace"])));
    rpc.send(json!({"type": "extension_ui_response", "id": select["id"], "value": "Grace"})).await;
    let notice = notified(&rpc, "hello Grace").await;
    assert_eq!(notice["notifyType"], "info");
    let status = rpc.wait_for(|record| record["method"] == "setStatus").await;
    assert_eq!((status["statusKey"].as_str(), status["statusText"].as_str()), (Some("greet"), Some("greeted Grace")));

    rpc.send(json!({"id": "p", "type": "prompt", "message": "hello"})).await;
    notified(&rpc, "source rpc").await;
    rpc.settled(1).await;
    rpc.finish().await;
}
