//! pi-rust-only events for hook runners: `instructions_loaded`, `prompt_expansion`,
//! `notification`, and `tool_call_blocked`. Upstream pi has none of them.

use std::sync::{Arc, Mutex};

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{NotifyLevel, PromptDisposition};
use serde_json::{Value, json};

use crate::extensions::harness::{Harness, Setup, eventually, harness};

/// An extension that records each event of `types` as a custom entry of that type.
fn recorder(types: &[&str], body: &str) -> String {
    let types = serde_json::to_string(types).unwrap();
    format!(
        "export default function (pi) {{\n\tfor (const type of {types}) pi.on(type, (event) => {{ pi.appendEntry(type, event); }});\n{body}\n}}\n"
    )
}

fn recorded(h: &Harness, kind: &str) -> Vec<Value> {
    h.entries().iter().filter(|entry| entry["customType"] == kind).map(|entry| entry["data"].clone()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn instructions_loaded_reports_each_context_file_after_session_start() {
    let h = harness(Setup {
        files: vec![("rules.ts", recorder(&["instructions_loaded"], ""))],
        load: vec!["rules.ts"],
        root_files: vec![("agent/AGENTS.md", "user rules".into()), ("work/CLAUDE.md", "project rules".into())],
        ..Setup::default()
    })
    .await;
    eventually(|| recorded(&h, "instructions_loaded").len() >= 2).await;
    let root = h.cwd.parent().unwrap().to_path_buf();
    let mine: Vec<Value> = recorded(&h, "instructions_loaded")
        .into_iter()
        .filter(|event| event["path"].as_str().is_some_and(|path| path.starts_with(&*root.to_string_lossy())))
        .collect();
    let event = |path: std::path::PathBuf, scope: &str| json!({"type": "instructions_loaded", "path": path.display().to_string(), "scope": scope, "reason": "startup"});
    assert_eq!(mine, vec![event(h.agent_dir.join("AGENTS.md"), "user"), event(h.cwd.join("CLAUDE.md"), "project")]);
}

const EXPANSION_FILES: [(&str, &str); 2] = [
    ("work/.pi/skills/deploy/SKILL.md", "---\nname: deploy\ndescription: Deploy the app\n---\nRun the deploy script."),
    ("work/.pi/prompts/review.md", "Review $1 carefully."),
];

fn expansion_setup(files: Vec<(&'static str, String)>, load: Vec<&'static str>, responses: usize) -> Setup {
    Setup {
        files,
        load,
        responses: (0..responses).map(|_| FauxResponse::text("ok")).collect(),
        root_files: EXPANSION_FILES.iter().map(|(path, content)| (*path, content.to_string())).collect(),
        ..Setup::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_expansion_reports_the_expansion_and_appends_context_texts() {
    let first = recorder(
        &["prompt_expansion"],
        "\tpi.on(\"prompt_expansion\", (event) => ({ action: \"context\", text: `first ${event.kind}` }));",
    );
    let second = "export default function (pi) {\n\tpi.on(\"prompt_expansion\", () => ({ action: \"context\", text: \"second\" }));\n}\n";
    let setup = expansion_setup(vec![("a.ts", first), ("b.ts", second.into())], vec!["a.ts", "b.ts"], 3);
    let h = harness(setup).await;
    h.session.prompt("/review main.rs", Vec::new()).await.unwrap();
    assert_eq!(h.last_prompt(), "Review main.rs carefully.\n\nfirst prompt_template\n\nsecond");
    h.session.prompt("/skill:deploy now", Vec::new()).await.unwrap();
    assert!(h.last_prompt().ends_with("now\n\nfirst skill\n\nsecond"), "{}", h.last_prompt());
    h.session.prompt("plain text", Vec::new()).await.unwrap();

    let events = recorded(&h, "prompt_expansion");
    assert_eq!(events.len(), 2, "plain text expands nothing: {events:?}");
    let review = h.cwd.join(".pi").join("prompts").join("review.md").display().to_string();
    assert_eq!(
        events[0],
        json!({"type": "prompt_expansion", "kind": "prompt_template", "name": "review", "args": "main.rs",
            "source": review, "prompt": "/review main.rs", "text": "Review main.rs carefully."})
    );
    let skill = &events[1];
    assert_eq!((&skill["kind"], &skill["name"], &skill["args"]), (&json!("skill"), &json!("deploy"), &json!("now")));
    assert_eq!(skill["prompt"], "/skill:deploy now");
    assert!(skill["source"].as_str().unwrap().replace('\\', "/").ends_with("skills/deploy/SKILL.md"), "{skill}");
    assert!(skill["text"].as_str().unwrap().contains("Run the deploy script."), "{skill}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_expansion_block_drops_the_prompt_with_a_warning() {
    let body = "\tpi.on(\"prompt_expansion\", (event) => event.name === \"review\" ? { action: \"block\", reason: \"no reviews\" } : { action: \"context\", text: \"late\" });";
    let setup = expansion_setup(vec![("block.ts", recorder(&["notification"], body))], vec!["block.ts"], 0);
    let h = harness(setup).await;
    let notices = Arc::new(Mutex::new(Vec::new()));
    let sink = notices.clone();
    h.session.set_notify_handler(Some(Arc::new(move |message: &str, level: NotifyLevel| {
        sink.lock().unwrap().push(format!("{level:?}: {message}"));
    })));
    let disposition = h.session.prompt_with("/review main.rs", Default::default()).await.unwrap();
    assert_eq!(disposition, PromptDisposition::Handled);
    assert!(h.provider.requests().is_empty(), "a blocked prompt sends no request");
    assert_eq!(notices.lock().unwrap().clone(), vec!["Warning: no reviews"]);
    eventually(|| !recorded(&h, "notification").is_empty()).await;
    assert_eq!(
        recorded(&h, "notification"),
        vec![json!({"type": "notification", "message": "no reviews", "level": "warning", "source": "pi"})]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn notification_reports_extension_notices_but_not_those_of_its_own_handlers() {
    let body = r#"
	pi.on("notification", (event, ctx) => { ctx.ui.notify(`echo ${event.message}`); });
	pi.registerCommand("say", { handler: async (_args, ctx) => ctx.ui.notify("hi", "warning") });
"#;
    let h = harness(Setup {
        files: vec![("notify.ts", recorder(&["notification"], body))],
        load: vec!["notify.ts"],
        ..Setup::default()
    })
    .await;
    h.session.prompt("/say", Vec::new()).await.unwrap();
    eventually(|| h.ui.calls().contains(&"notify(info): echo hi".to_string())).await;
    assert_eq!(h.ui.calls(), vec!["notify(warning): hi", "notify(info): echo hi"]);
    assert_eq!(
        recorded(&h, "notification"),
        vec![json!({"type": "notification", "message": "hi", "level": "warning", "source": "extension"})]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_blocked_names_the_blocking_extension() {
    let body = r#"
	pi.registerTool({
		name: "probe", label: "Probe", description: "Echo", parameters: { type: "object", properties: { text: { type: "string" } } },
		async execute() { return { content: [{ type: "text", text: "ran" }], details: {} }; },
	});
	pi.on("tool_call", (event) => event.input.text === "secret" ? { block: true, reason: "not allowed" } : undefined);
"#;
    let h = harness(Setup {
        files: vec![("guard.ts", recorder(&["tool_call_blocked"], body))],
        load: vec!["guard.ts"],
        responses: vec![
            FauxResponse::tool_call("c1", "probe", json!({"text": "secret"})),
            FauxResponse::tool_call("c2", "probe", json!({"text": "open"})),
            FauxResponse::text("done"),
        ],
        ..Setup::default()
    })
    .await;
    h.session.prompt("go", Vec::new()).await.unwrap();
    eventually(|| !recorded(&h, "tool_call_blocked").is_empty()).await;
    let extension = h.extension_dir.join("guard.ts").display().to_string();
    assert_eq!(
        recorded(&h, "tool_call_blocked"),
        vec![json!({"type": "tool_call_blocked", "toolCallId": "c1", "toolName": "probe", "input": {"text": "secret"},
            "reason": "not allowed", "source": "extension", "extension": extension})]
    );
}
