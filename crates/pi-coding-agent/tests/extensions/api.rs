//! The `ExtensionAPI` (`pi.*`) methods, each checked through the session or the runner.

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::markdown_transform::{MarkdownMessageType, MarkdownTransformContext};
use pi_tui::Component;
use serde_json::{Value, json};

use crate::harness::{Setup, eventually, harness};

fn probe_extension() -> String {
    let exe = std::env::current_exe().unwrap().display().to_string();
    let exe = serde_json::to_string(&exe).unwrap();
    format!(
        r#"import {{ Text }} from "@earendil-works/pi-tui";
export default function (pi) {{
	pi.registerCommand("probe", {{
		description: "Probe the API",
		getArgumentCompletions: (prefix) =>
			["alpha", "beta"].filter((v) => v.startsWith(prefix)).map((v) => ({{ value: v, label: v.toUpperCase() }})),
		handler: async (args, ctx) => {{
			pi.setSessionName("Named by extension");
			pi.appendEntry("probe-state", {{ args }});
			pi.setLabel(ctx.sessionManager.getLeafId(), "checkpoint");
			const result = await pi.exec({exe}, ["--list"]);
			ctx.ui.notify(`exec ${{result.code}} ${{result.stdout.includes(": test")}}`);
			pi.setActiveTools(pi.getActiveTools().filter((name) => name !== "write"));
			ctx.ui.notify(`all ${{pi.getAllTools().some((tool) => tool.name === "write")}}`);
			ctx.ui.notify(`thinking ${{pi.getThinkingLevel()}}`);
			pi.setThinkingLevel("high");
			ctx.ui.notify(`thinking now ${{pi.getThinkingLevel()}}`);
			ctx.ui.notify(`model ${{await pi.setModel(ctx.modelRegistry.find("local", "other"))}}`);
			ctx.ui.notify(`name ${{pi.getSessionName()}}`);
			ctx.ui.notify(`commands ${{pi.getCommands().map((command) => command.name).join(",")}}`);
		}},
	}});
	pi.registerShortcut("ctrl+shift+p", {{ description: "Probe", handler: async (ctx) => ctx.ui.notify("shortcut ran") }});
	pi.registerShortcut("ctrl+c", {{ description: "Reserved", handler: () => {{}} }});
	pi.registerMessageRenderer("probe", (message, options) => new Text(`[${{message.content}}|${{options.expanded}}]`, 0, 0));
	pi.registerMarkdownTransformer((markdown, context) =>
		markdown.replace("foo", `bar(${{context.messageType}} ${{context.messageId}} ${{context.timestamp}})`),
	);
}}
"#
    )
}

fn message_text(value: &Value) -> String {
    match &value["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|block| block["text"].as_str()).collect(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn api_methods_reach_the_session() {
    let h = harness(Setup {
        files: vec![("probe.ts", probe_extension())],
        load: vec!["probe.ts"],
        settings: json!({"defaultThinkingLevel": "low"}),
        ..Setup::default()
    })
    .await;
    let disposition = h.session.prompt("/probe x", Vec::new()).await.unwrap();
    assert_eq!(disposition.as_str(), "handled");
    assert_eq!(h.errors(), Vec::<String>::new());
    assert_eq!(
        h.ui.calls()[..6],
        [
            "notify(info): exec 0 true",
            "notify(info): all true",
            "notify(info): thinking low",
            "notify(info): thinking now high",
            "notify(info): model true",
            "notify(info): name Named by extension",
        ]
    );
    // Skills of the user's home directory may follow the extension command.
    assert!(h.ui.calls()[6].starts_with("notify(info): commands probe"), "{:?}", h.ui.calls());
    assert_eq!(h.session.session_name().as_deref(), Some("Named by extension"));
    let entries = h.entries();
    let custom = entries.iter().find(|entry| entry["customType"] == "probe-state").expect("custom entry");
    assert_eq!(custom["data"], json!({"args": "x"}));
    {
        let manager = h.session.session_manager();
        let ids: Vec<&str> = entries.iter().filter_map(|entry| entry["id"].as_str()).collect();
        let label = ids.iter().find_map(|id| manager.get_label(id).map(str::to_string));
        assert_eq!(label.as_deref(), Some("checkpoint"));
    }
    assert!(!h.session.active_tool_names().contains(&"write".to_string()));
    // The other model has no reasoning, so switching to it turns thinking off.
    assert_eq!(h.session.thinking_level().as_str(), "off");
    assert_eq!(h.session.model().map(|model| model.id), Some("other".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_reports_why_a_command_could_not_start() {
    let source = r#"export default function (pi) {
	pi.registerCommand("missing", { handler: async (_args, ctx) => {
		const result = await pi.exec("pi-test-missing-binary", [], { timeout: 5000 });
		ctx.ui.notify(`exec ${result.code} ${result.killed} ${result.stdout === ""} ${result.stderr}`);
	} });
}
"#;
    let h = harness(Setup { files: vec![("missing.ts", source.into())], load: vec!["missing.ts"], ..Setup::default() })
        .await;
    h.session.prompt("/missing", Vec::new()).await.unwrap();
    let calls = h.ui.calls();
    let [call] = calls.as_slice() else { panic!("{calls:?}") };
    // A command that is not installed must differ from one that exits 1 without output.
    assert!(call.starts_with("notify(info): exec 1 false true "), "{call}");
    assert!(call.contains("ENOENT"), "{call}");
}

/// The JSON result of `pi.exec(command, args)` run from an extension command.
#[cfg(unix)]
async fn exec_result(command: &str, args: &[&str]) -> Value {
    let call = format!("pi.exec({}, {})", json!(command), json!(args));
    let source = format!(
        r#"export default function (pi) {{
	pi.registerCommand("run", {{ handler: async (_args, ctx) => ctx.ui.notify(JSON.stringify(await {call})) }});
}}
"#
    );
    let h = harness(Setup { files: vec![("run.ts", source)], load: vec!["run.ts"], ..Setup::default() }).await;
    h.session.prompt("/run", Vec::new()).await.unwrap();
    let calls = h.ui.calls();
    let [call] = calls.as_slice() else { panic!("{calls:?}") };
    serde_json::from_str(call.strip_prefix("notify(info): ").expect("notify")).unwrap()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn exec_keeps_stderr_empty_when_a_command_exits_silently() {
    let result = exec_result("sh", &["-c", "exit 1"]).await;
    assert_eq!(result, json!({"stdout": "", "stderr": "", "code": 1, "killed": false}));
}

// Upstream #11312: a character split across two pipe chunks came back as U+FFFD.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn exec_decodes_output_across_utf8_chunk_boundaries() {
    let split = r"printf '\344'; sleep 0.1; printf '\275\240\345\245\275'";
    let script = format!("{split}; ({split}) >&2");
    let result = exec_result("sh", &["-c", &script]).await;
    assert_eq!(result, json!({"stdout": "你好", "stderr": "你好", "code": 0, "killed": false}));
}

// Upstream issue 11360: unbounded output could exhaust the string limit and crash the host.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn exec_stops_a_command_whose_output_exceeds_the_cap() {
    let source = r#"export default function (pi) {
	pi.registerCommand("big", { handler: async (_args, ctx) => {
		const big = await pi.exec("yes", ["0123456789abcdef0123456789abcdef"]);
		const small = await pi.exec("sh", ["-c", "printf ok"]);
		ctx.ui.notify(JSON.stringify({ length: big.stdout.length, code: big.code, killed: big.killed, stderr: big.stderr, small: small.stdout }));
	} });
}
"#;
    let h = harness(Setup { files: vec![("big.ts", source.into())], load: vec!["big.ts"], ..Setup::default() }).await;
    h.session.prompt("/big", Vec::new()).await.unwrap();
    let calls = h.ui.calls();
    let [call] = calls.as_slice() else { panic!("{calls:?}") };
    let result: Value = serde_json::from_str(call.strip_prefix("notify(info): ").expect("notify")).unwrap();
    let cap = 16 * 1024 * 1024;
    assert_eq!(result["length"], cap);
    assert_ne!(result["code"], 0);
    assert_eq!(result["killed"], true);
    assert_eq!(result["stderr"], format!("[output exceeded {cap} bytes; output is incomplete]"));
    assert_eq!(result["small"], "ok", "the next command still runs");
}

#[tokio::test(flavor = "multi_thread")]
async fn completions_shortcuts_and_renderers_come_from_the_registry() {
    let h = harness(Setup { files: vec![("probe.ts", probe_extension())], load: vec!["probe.ts"], ..Setup::default() })
        .await;
    let items = h.runner.command_completions("probe", "a").await.expect("completions");
    assert_eq!(items, vec![json!({"value": "alpha", "label": "ALPHA"})]);

    let (shortcuts, warnings) = h.runner.shortcuts();
    let keys: Vec<&str> = shortcuts.iter().map(|shortcut| shortcut.shortcut.as_str()).collect();
    assert_eq!(keys, vec!["ctrl+shift+p"]);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("'ctrl+c'") && warnings[0].ends_with("Skipping."), "{warnings:?}");
    let path = shortcuts[0].extension_path.clone();
    assert!(h.runner.run_shortcut(&path, "ctrl+shift+p").await);
    assert_eq!(h.ui.calls(), vec!["notify(info): shortcut ran"]);

    let message = json!({"role": "custom", "customType": "probe", "content": "hello", "display": true});
    let runner = h.runner.clone();
    let lines = tokio::task::spawn_blocking(move || {
        let mut component = runner.render_message("probe", &message, true, 1).expect("rendered");
        component.render(40)
    })
    .await
    .unwrap();
    assert_eq!(lines.first().map(|line| line.trim_end()), Some("[hello|true]"));
    assert!(h.runner.render_message("other", &json!({}), false, 1).is_none());

    let transformer = h.runner.markdown_transformer().expect("transformer");
    let live = MarkdownTransformContext {
        message_type: MarkdownMessageType::Assistant,
        is_streaming: false,
        available_width: 80,
        message_id: None,
        timestamp: None,
    };
    let restored = MarkdownTransformContext {
        message_id: Some("entry-abc".into()),
        timestamp: Some("2025-01-15T10:30:00.000Z".into()),
        ..live.clone()
    };
    let transformed =
        tokio::task::spawn_blocking(move || [transformer("a foo b", &live), transformer("foo", &restored)])
            .await
            .unwrap();
    assert_eq!(transformed[0].as_deref(), Some("a bar(assistant undefined undefined) b"));
    assert_eq!(transformed[1].as_deref(), Some("bar(assistant entry-abc 2025-01-15T10:30:00.000Z)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_from_extensions_trigger_turns_or_wait_for_the_next_one() {
    let source = r#"export default function (pi) {
	pi.registerCommand("note", { handler: async () => {
		pi.sendMessage({ customType: "note", content: "remember this", display: true }, { triggerTurn: true });
	} });
	pi.registerCommand("later", { handler: async () => {
		pi.sendMessage({ customType: "later", content: "for the next turn", display: false }, { deliverAs: "nextTurn" });
	} });
	pi.registerCommand("ask", { handler: async () => { pi.sendUserMessage("question from extension"); } });
	pi.on("input", (event) => { if (event.source === "extension") pi.appendEntry("seen", { text: event.text }); });
}
"#;
    let h = harness(Setup {
        files: vec![("messages.ts", source.into())],
        load: vec!["messages.ts"],
        responses: vec![FauxResponse::text("one"), FauxResponse::text("two"), FauxResponse::text("three")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("/note", Vec::new()).await.unwrap();
    eventually(|| h.provider.requests().len() == 1).await;
    h.session.wait_for_idle().await;
    assert_eq!(h.last_prompt(), "remember this");

    h.session.prompt("/later", Vec::new()).await.unwrap();
    assert_eq!(h.provider.requests().len(), 1, "nextTurn waits for a prompt");
    h.session.prompt("go", Vec::new()).await.unwrap();
    let requests = h.provider.requests();
    let texts: Vec<String> =
        requests[1].messages.iter().map(|message| message_text(&serde_json::to_value(message).unwrap())).collect();
    // Upstream sends `nextTurn` messages right after the user message.
    assert_eq!(texts[texts.len() - 2..], ["go", "for the next turn"], "{texts:?}");

    h.session.prompt("/ask", Vec::new()).await.unwrap();
    eventually(|| h.provider.requests().len() == 3).await;
    h.session.wait_for_idle().await;
    assert_eq!(h.last_prompt(), "question from extension");
    let seen = h.entries().into_iter().find(|entry| entry["customType"] == "seen").expect("input event");
    assert_eq!(seen["data"]["text"], "question from extension");
}
