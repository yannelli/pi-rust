//! Host features around extensions: inline Rust extensions, virtual models, MCP server
//! registration, and replacing built-in extensions.

use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, SharedTool, ToolUpdateCallback};
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::AgentSession;
use pi_coding_agent::extensions::InlineExtension;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::harness::{Setup, harness};

struct Shout;

#[async_trait]
impl AgentTool for Shout {
    fn name(&self) -> &str {
        "shout"
    }

    fn description(&self) -> &str {
        "Upper-case the text"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok(AgentToolResult::text(params["text"].as_str().unwrap_or_default().to_uppercase()))
    }
}

/// Prefixes every prompt and supplies the `shout` tool.
struct Prefixer;

#[async_trait]
impl InlineExtension for Prefixer {
    fn name(&self) -> &str {
        "prefixer"
    }

    fn events(&self) -> Vec<String> {
        vec!["input".into()]
    }

    async fn on_event(&self, event: Value, session: Option<Arc<AgentSession>>) -> Option<Value> {
        let named = session.is_some_and(|session| session.session_file().is_some());
        Some(json!({"action": "transform", "text": format!("[{named}] {}", event["text"].as_str()?)}))
    }

    fn tools(&self) -> Vec<SharedTool> {
        vec![Arc::new(Shout)]
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_extensions_handle_events_and_register_tools() {
    let h = harness(Setup {
        inline: vec![Arc::new(Prefixer)],
        responses: vec![FauxResponse::tool_call("c1", "shout", json!({"text": "hey"})), FauxResponse::text("done")],
        ..Setup::default()
    })
    .await;
    assert_eq!(h.runner.extension_paths(), vec!["<inline:prefixer>"]);
    assert!(h.session.active_tool_names().contains(&"shout".to_string()));
    h.session.prompt("hello", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(transcript.contains(&"user: [true] hello".to_string()), "{transcript:?}");
    assert!(transcript.contains(&"toolResult: HEY".to_string()), "{transcript:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn virtual_models_route_requests_through_extensions() {
    let source = r#"export default function (pi) {
	pi.registerVirtualModel({
		provider: "local", id: "auto", name: "Auto", contextWindow: 1000, maxTokens: 100,
		route: (request, ctx) => ({
			model: ctx.modelRegistry.find("local", request.messages.length > 2 ? "other" : "tiny"),
			thinkingLevel: "off",
			state: { calls: (request.state?.calls ?? 0) + 1 },
		}),
	});
}
"#;
    let h = harness(Setup {
        files: vec![("router.ts", source.into())],
        load: vec!["router.ts"],
        responses: vec![FauxResponse::text("first"), FauxResponse::text("second")],
        ..Setup::default()
    })
    .await;
    let auto = h.session.model_runtime().registry().find("local", "auto").cloned().expect("virtual model listed");
    assert_eq!(auto.name, "Auto");
    h.session.set_model(auto, false).await.unwrap();
    h.session.prompt("one", Vec::new()).await.unwrap();
    h.session.prompt("two", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(transcript.ends_with(&["assistant: second".to_string()]), "{transcript:?}");
    let models: Vec<String> = h
        .session
        .messages()
        .iter()
        .filter_map(|message| match message {
            pi_ai::Message::Assistant(assistant) => {
                Some(assistant.response_model.clone().unwrap_or(assistant.model.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(models, vec!["tiny", "other"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_servers_register_and_replacing_builtin_mcp_reports_unconnected_ones() {
    let source = r#"export default function (pi) {
	pi.registerMcpServer("docs", { command: "docs-server", args: ["--stdio"] });
	pi.registerCommand("mcp", { description: "My MCP", handler: async (_args, ctx) => {
		ctx.ui.notify(pi.getMcpServers().map((server) => server.name).join(","));
	} });
}
"#;
    let h = harness(Setup { files: vec![("mcp.ts", source.into())], load: vec!["mcp.ts"], ..Setup::default() }).await;
    let servers = h.runner.mcp_registry().list();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "docs");
    assert!(!h.session.builtin_extension_enabled("mcp"), "the /mcp command replaces the built-in extension");
    let errors = h.errors();
    assert!(errors.iter().any(|error| error.starts_with("register_mcp_server: ")), "{errors:?}");
    h.session.prompt("/mcp", Vec::new()).await.unwrap();
    assert_eq!(h.ui.calls(), vec!["notify(info): docs"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_tool_replaces_the_builtin_codemode_tool() {
    let source = r#"import { Type } from "typebox";
export default function (pi) {
	pi.registerTool({ name: "codemode", label: "Mine", description: "Replacement", parameters: Type.Object({}),
		async execute() { return { content: [{ type: "text", text: "replaced" }] }; } });
}
"#;
    let h =
        harness(Setup { files: vec![("codemode.ts", source.into())], load: vec!["codemode.ts"], ..Setup::default() })
            .await;
    assert!(!h.session.builtin_extension_enabled("codemode"));
    let replacement = h.runner.registry().builtin_replacement("codemode").expect("replacement");
    assert!(replacement.warning("codemode").starts_with("Extension "), "{replacement:?}");
    let tools = h.session.all_tools();
    let codemode: Vec<_> = tools.iter().filter(|tool| tool.name == "codemode").collect();
    assert_eq!(codemode.len(), 1);
    assert_eq!(codemode[0].description, "Replacement");
}
