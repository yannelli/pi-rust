//! Loading: order, failures, async factories, directories and manifests, settings entries,
//! flags, and the `project_trust` pass.

use std::sync::Arc;

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{CreateServicesOptions, create_agent_session_services};
use pi_coding_agent::app::{AppOptions, extension_trust_decision};
use pi_coding_agent::extension_ui::ExtensionMode;
use pi_coding_agent::extensions::{
    LoadOptions, discover_extensions_in_dir, load_extensions, resolve_extension_entries,
};
use pi_coding_agent::settings::SettingsManager;
use serde_json::json;

use crate::harness::{RecordingUi, Setup, harness};

fn transform(suffix: &str) -> String {
    format!(
        "export default function (pi) {{\n\tpi.on(\"input\", (event) => ({{ action: \"transform\", text: event.text + \" {suffix}\" }}));\n}}\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn handlers_run_in_load_order_and_failures_stay_with_their_extension() {
    let failing =
        "export default function (pi) {\n\tpi.on(\"before_agent_start\", () => { throw new Error(\"nope\"); });\n}\n";
    let h = harness(Setup {
        files: vec![
            ("a.ts", transform("A")),
            ("broken.ts", "export default function () { throw new Error(\"load failure\"); }\n".into()),
            ("b.ts", transform("B")),
            ("failing.ts", failing.into()),
        ],
        load: vec!["a.ts", "broken.ts", "b.ts", "failing.ts"],
        responses: vec![FauxResponse::text("ok")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("x", Vec::new()).await.unwrap();
    assert_eq!(h.last_prompt(), "x A B");
    let errors = h.errors();
    assert!(errors.iter().any(|error| error.starts_with("load: ") && error.contains("load failure")), "{errors:?}");
    assert!(errors.iter().any(|error| error == "before_agent_start: nope"), "{errors:?}");
    assert_eq!(h.runner.extension_paths().len(), 3, "{:?}", h.runner.extension_paths());
}

#[tokio::test(flavor = "multi_thread")]
async fn async_factories_finish_before_the_session_and_handlers_unsubscribe() {
    let source = r#"export default async function (pi) {
	await new Promise((resolve) => setTimeout(resolve, 20));
	const off = pi.on("input", () => ({ action: "transform", text: "removed" }));
	off();
	pi.on("input", (event) => ({ action: "transform", text: event.text + "!" }));
}
"#;
    let h = harness(Setup {
        files: vec![("async.ts", source.into())],
        load: vec!["async.ts"],
        responses: vec![FauxResponse::text("ok")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("hi", Vec::new()).await.unwrap();
    assert_eq!(h.last_prompt(), "hi!");
    assert_eq!(h.errors(), Vec::<String>::new());
}

#[test]
fn directories_resolve_through_manifests_and_index_files() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    std::fs::create_dir_all(dir.join("pkg/src")).unwrap();
    std::fs::write(dir.join("pkg/package.json"), json!({"pi": {"extensions": ["./src/main.ts"]}}).to_string()).unwrap();
    std::fs::write(dir.join("pkg/src/main.ts"), "export default function () {}\n").unwrap();
    std::fs::create_dir_all(dir.join("plain")).unwrap();
    std::fs::write(dir.join("plain/index.ts"), "export default function () {}\n").unwrap();
    std::fs::write(dir.join("single.js"), "export default function () {}\n").unwrap();
    std::fs::write(dir.join("notes.md"), "not an extension\n").unwrap();
    assert_eq!(resolve_extension_entries(&dir.join("pkg")), Some(vec![dir.join("pkg/src/main.ts")]));
    assert_eq!(resolve_extension_entries(&dir.join("plain")), Some(vec![dir.join("plain/index.ts")]));
    let mut found = discover_extensions_in_dir(dir);
    found.sort();
    assert_eq!(found, vec![dir.join("pkg/src/main.ts"), dir.join("plain/index.ts"), dir.join("single.js")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_entries_load_and_builtin_entries_are_left_to_the_host() {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let file = root.path().join("tool.ts");
    let source = r#"import { Type } from "typebox";
export default function (pi) {
	pi.registerTool({ name: "from_settings", label: "x", description: "x", parameters: Type.Object({}),
		async execute() { return { content: [{ type: "text", text: "ok" }] }; } });
}
"#;
    std::fs::write(&file, source).unwrap();
    let settings = json!({"extensions": [file.display().to_string(), "builtin:mcp"]});
    let services = create_agent_session_services(CreateServicesOptions {
        settings: Some(Arc::new(SettingsManager::in_memory(settings))),
        ..CreateServicesOptions::new(&cwd, &agent_dir)
    })
    .await;
    let loaded = load_extensions(&services, &LoadOptions::default()).await;
    assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    let runner = loaded.runner.expect("runner");
    assert_eq!(runner.extension_paths(), vec![file.display().to_string()]);
    assert!(runner.registry().tools.iter().any(|tool| tool.name == "from_settings"));
    runner.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn flags_and_the_session_start_event_reach_extensions() {
    let source = r#"export default function (pi) {
	pi.registerFlag("plan", { description: "Plan first", type: "boolean", default: false });
	pi.registerFlag("level", { type: "string", default: "low" });
	pi.on("session_start", (_event, ctx) => ctx.ui.notify(`plan=${pi.getFlag("plan")} level=${pi.getFlag("level")}`));
}
"#;
    let h = harness(Setup {
        files: vec![("flags.ts", source.into())],
        load: vec!["flags.ts"],
        flags: vec![("plan", json!(true))],
        ..Setup::default()
    })
    .await;
    assert_eq!(h.ui.calls(), vec!["notify(info): plan=true level=low"]);
    let start = json!({"type": "session_start", "reason": "resume", "previousSessionFile": "/old.jsonl"});
    let source = "export default function (pi) {\n\tpi.on(\"session_start\", (event, ctx) => ctx.ui.notify(`${event.reason} ${event.previousSessionFile}`));\n}\n";
    let resumed = harness(Setup {
        files: vec![("start.ts", source.into())],
        load: vec!["start.ts"],
        start_event: Some(start),
        ..Setup::default()
    })
    .await;
    assert_eq!(resumed.ui.calls(), vec!["notify(info): resume /old.jsonl"]);
    let registry = h.runner.registry();
    let flag = registry.flag("plan").expect("plan flag");
    assert_eq!((flag.kind.as_str(), flag.description.as_deref()), ("boolean", Some("Plan first")));
}

#[tokio::test(flavor = "multi_thread")]
async fn project_trust_handlers_decide_with_the_startup_ui() {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(cwd.join(".pi/extensions")).unwrap();
    std::fs::write(cwd.join(".pi/extensions/project.ts"), "export default function () {}\n").unwrap();
    std::fs::create_dir_all(agent_dir.join("extensions")).unwrap();
    let source = r#"export default function (pi) {
	pi.on("project_trust", async (event, ctx) => {
		const choice = await ctx.ui.select(`Trust ${event.cwd}? (${ctx.mode}, ${ctx.hasUI})`, ["Trust", "Skip"]);
		return choice === "Trust" ? { trusted: "yes", remember: true } : undefined;
	});
}
"#;
    std::fs::write(agent_dir.join("extensions/trust.ts"), source).unwrap();
    let ui = Arc::new(RecordingUi::default());
    ui.answers.lock().unwrap().push("Trust".into());
    let options = AppOptions {
        agent_dir: agent_dir.clone(),
        project_trust_ui: Some((ui.clone(), ExtensionMode::Tui)),
        ..AppOptions::default()
    };
    let (decision, diagnostics) = extension_trust_decision(&options, &cwd).await;
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let decision = decision.expect("a decision");
    assert!(decision.trusted && decision.remember);
    assert_eq!(ui.calls(), vec![format!("select: Trust {}? (tui, true) [Trust, Skip]", cwd.display())]);
    let skipped = AppOptions { project_trust_override: Some(false), ..options };
    assert!(extension_trust_decision(&skipped, &cwd).await.0.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn extensions_see_the_child_process_environment_overlay() {
    pi_ai::env::set_child_process_env("PI_EXTENSION_OVERLAY_TEST", "proxy-from-settings");
    let source = "export default function (pi) {\n\tpi.on(\"input\", (event) => ({ action: \"transform\", text: event.text + \" \" + process.env.PI_EXTENSION_OVERLAY_TEST }));\n}\n";
    let h = harness(Setup {
        files: vec![("env.ts", source.into())],
        load: vec!["env.ts"],
        responses: vec![FauxResponse::text("ok")],
        ..Setup::default()
    })
    .await;
    h.session.prompt("x", Vec::new()).await.unwrap();
    assert_eq!(h.last_prompt(), "x proxy-from-settings");
}
