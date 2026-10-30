//! Native extensions: the bridge starts `tests/fixtures/native/fixture.mjs` as a child process
//! (the pi binary in its Node-style mode) and talks the native protocol to it over stdio.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extension_ui::{
    DialogOptions, ExtensionMode, ExtensionUi, NoOpExtensionUi, NotifyType, WidgetPlacement,
};
use pi_coding_agent::extensions::{ComponentFactory, CustomRequest, ExtensionBindings};
use pi_tui::Component;
use serde_json::{Value, json};

use crate::harness::{Harness, Setup, eventually, harness};

#[derive(Default)]
struct NativeUi {
    calls: Mutex<Vec<String>>,
    answers: Mutex<Vec<String>>,
    widgets: Mutex<Vec<(String, ComponentFactory)>>,
}

impl NativeUi {
    fn push(&self, line: String) {
        self.calls.lock().unwrap().push(line);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn widget(&self, key: &str) -> Option<ComponentFactory> {
        let widgets = self.widgets.lock().unwrap();
        widgets.iter().rev().find(|(name, _)| name == key).map(|(_, factory)| factory.clone())
    }
}

/// Render `component` until a line contains `text`.
fn render_until(component: &mut dyn Component, width: usize, text: &str) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let lines = component.render(width);
        if lines.iter().any(|line| line.contains(text)) {
            return lines;
        }
        assert!(std::time::Instant::now() < deadline, "no line with {text:?} in {lines:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[async_trait]
impl ExtensionUi for NativeUi {
    async fn select(&self, title: &str, options: &[String], _opts: DialogOptions) -> Option<String> {
        self.push(format!("select: {title} [{}]", options.join(", ")));
        let mut answers = self.answers.lock().unwrap();
        (!answers.is_empty()).then(|| answers.remove(0))
    }

    async fn confirm(&self, _title: &str, _message: &str, _opts: DialogOptions) -> bool {
        false
    }

    async fn input(&self, _title: &str, _placeholder: Option<&str>, _opts: DialogOptions) -> Option<String> {
        None
    }

    async fn editor(&self, _title: &str, _prefill: Option<&str>, _opts: DialogOptions) -> Option<String> {
        None
    }

    fn notify(&self, message: &str, kind: Option<NotifyType>) {
        self.push(format!("notify({}): {message}", kind.unwrap_or_default().as_str()));
    }

    fn set_status(&self, _key: &str, _text: Option<&str>) {}

    fn set_widget(&self, key: &str, lines: Option<&[String]>, _placement: Option<WidgetPlacement>) {
        self.push(format!("widget: {key}={}", lines.map(|lines| lines.join("|")).unwrap_or_default()));
    }

    fn set_widget_component(&self, key: &str, factory: Option<ComponentFactory>, _placement: Option<WidgetPlacement>) {
        if let Some(factory) = factory {
            self.widgets.lock().unwrap().push((key.to_string(), factory));
        }
    }

    fn set_title(&self, _title: &str) {}

    fn set_editor_text(&self, _text: &str) {}

    fn write_terminal(&self, _data: &str, _tmux_passthrough: bool) -> bool {
        true
    }

    async fn custom(&self, request: CustomRequest) -> Result<Option<Value>, String> {
        let mut component = request.factory.create_async().await?;
        let lines = tokio::task::block_in_place(|| {
            let first = component.render(30);
            let resized = render_until(&mut component, 30, "width 30");
            component.handle_input("q");
            [first, resized].concat()
        });
        self.push(format!("panel: {}", lines.join("|")));
        Ok(Some(request.done.await.map_err(|error| error.to_string())?))
    }
}

fn package_json() -> String {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native/fixture.mjs");
    json!({"name": "native-fixture", "pi": {"native": [{
        "command": env!("CARGO_BIN_EXE_pi"),
        "args": [fixture.display().to_string()],
        "env": {"PI_JS_FORK": "1"},
    }]}})
    .to_string()
}

/// Records the numeric keys of `tool_call` events after the native handler ran.
const OBSERVER: &str = r#"export default function (pi) {
	pi.on("tool_call", (event) => {
		if (event.input?.text === "array") pi.appendEntry("array-keys", { keys: Object.keys(event).filter((key) => /^\d+$/.test(key)) });
	});
}"#;

async fn native_with(
    responses: Vec<FauxResponse>,
    mode: ExtensionMode,
    ui: Arc<dyn ExtensionUi>,
    observer: bool,
) -> Harness {
    let mut files = vec![("native-pkg/package.json", package_json())];
    let mut load = vec!["native-pkg"];
    if observer {
        files.push(("observer.js", OBSERVER.to_string()));
        load.push("observer.js");
    }
    let h = harness(Setup {
        files,
        load,
        from_settings: true,
        flags: vec![("native-flag", json!("on"))],
        responses,
        bind: false,
        ..Setup::default()
    })
    .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let errors = h.errors.clone();
    let bindings = ExtensionBindings {
        ui,
        mode,
        on_error: Some(Arc::new(move |error| errors.lock().unwrap().push(error.clone()))),
        ..ExtensionBindings::default()
    };
    h.session.bind_extensions(bindings).await;
    h
}

async fn native(responses: Vec<FauxResponse>) -> (Harness, Arc<NativeUi>) {
    let ui = Arc::new(NativeUi::default());
    (native_with(responses, ExtensionMode::Tui, ui.clone(), false).await, ui)
}

fn entry_data(h: &Harness, custom_type: &str) -> Option<Value> {
    h.entries().iter().find(|entry| entry["customType"] == custom_type).map(|entry| entry["data"].clone())
}

fn texts(h: &Harness, kind: &str) -> Vec<String> {
    h.events_of(kind).iter().map(|event| event.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_events_and_widgets_cross_the_process_boundary() {
    let (h, ui) = native(vec![
        FauxResponse::tool_call("c1", "native_echo", json!({"text": "hi"})),
        FauxResponse::tool_call("c2", "native_echo", json!({"text": "blocked"})),
        FauxResponse::text("done"),
    ])
    .await;
    assert_eq!(h.runner.extension_paths(), vec!["native:native-fixture/fixture.mjs"]);
    assert!(h.session.active_tool_names().contains(&"native_echo".to_string()));

    eventually(|| ui.widget("native").is_some()).await;
    eventually(|| ui.calls().contains(&"notify(info): native ready".to_string())).await;
    let factory = ui.widget("native").unwrap();
    let widget = tokio::task::block_in_place(|| {
        let mut component = factory.create().unwrap();
        let first = component.render(40);
        let resized = render_until(&mut component, 40, "width 40");
        [first, resized]
    });
    assert_eq!(widget, [vec!["widget for tui".to_string(), "flag=on".into()], vec!["width 40".into()]]);

    h.session.prompt("hello", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(transcript.contains(&"toolResult: echo: hi!".to_string()), "{transcript:?}");
    assert!(transcript.iter().any(|line| line.contains("blocked by native")), "{transcript:?}");
    let updates = texts(&h, "tool_execution_update");
    assert!(updates.iter().any(|update| update.contains("working on hi!")), "{updates:?}");
    let system = pi_ai::transcript::flatten_context(&h.provider.requests()[0]).system_prompt.unwrap_or_default();
    assert!(system.contains("From the native fixture."), "{system}");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_reach_dialogs_panels_and_the_model() {
    let (h, ui) = native(vec![FauxResponse::text("llm answer")]).await;
    ui.answers.lock().unwrap().push("blue".into());
    h.session.prompt("/native-ask", Vec::new()).await.unwrap();
    h.session.prompt("/native-panel", Vec::new()).await.unwrap();
    h.session.prompt("/native-llm", Vec::new()).await.unwrap();
    let data = |custom_type: &str| entry_data(&h, custom_type);
    assert_eq!(data("native-answer"), Some(json!({"answer": "blue"})));
    assert_eq!(data("native-panel"), Some(json!({"result": "closed by q"})));
    assert_eq!(data("native-llm"), Some(json!({"text": "llm answer"})));
    let calls = ui.calls();
    assert!(calls.contains(&"select: Pick one [red, blue]".to_string()), "{calls:?}");
    assert!(calls.contains(&"panel: Panel one|width 30".to_string()), "{calls:?}");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[tokio::test(flavor = "multi_thread")]
async fn aborting_a_tool_call_cancels_the_request() {
    let (h, _ui) = native(vec![FauxResponse::tool_call("c1", "native_echo", json!({"text": "x", "wait": true}))]).await;
    let session = h.session.clone();
    let run = tokio::spawn(async move { session.prompt("go", Vec::new()).await });
    eventually(|| !h.events_of("tool_execution_update").is_empty()).await;
    h.session.abort().await;
    let _ = run.await.unwrap();
    let ends = texts(&h, "tool_execution_end");
    assert!(ends.iter().any(|end| end.contains("cancelled by host")), "{ends:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_child_is_reported_once_and_its_tools_deactivate() {
    let (h, ui) = native(Vec::new()).await;
    h.session.prompt("/native-crash", Vec::new()).await.unwrap();
    let exited = |calls: &[String]| calls.iter().filter(|call| call.contains("exited with code 3")).count();
    eventually(|| exited(&ui.calls()) == 1).await;
    assert!(
        ui.calls()
            .contains(&"notify(error): Native extension native:native-fixture/fixture.mjs exited with code 3".into())
    );
    eventually(|| !h.session.active_tool_names().contains(&"native_echo".to_string())).await;
    assert!(h.errors().iter().any(|error| error.contains("exited with code 3")), "{:?}", h.errors());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(exited(&ui.calls()), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn patches_reach_tool_results_messages_and_context() {
    let h = native_with(
        vec![
            FauxResponse::tool_call("c1", "native_echo", json!({"text": "patch"})),
            FauxResponse::tool_call("c2", "native_echo", json!({"text": "array"})),
            FauxResponse::text("done patch"),
        ],
        ExtensionMode::Tui,
        Arc::new(NativeUi::default()),
        true,
    )
    .await;
    h.session.prompt("patch context", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(transcript.contains(&"toolResult: patched result".to_string()), "{transcript:?}");
    assert!(transcript.contains(&"toolResult: echo: array".to_string()), "{transcript:?}");
    assert!(transcript.contains(&"assistant: message patched".to_string()), "{transcript:?}");
    let request = serde_json::to_string(&h.provider.requests()[0].messages).unwrap();
    assert!(request.contains("context patched"), "{request}");
    assert_eq!(entry_data(&h, "array-keys"), Some(json!({"keys": []})));
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[tokio::test(flavor = "multi_thread")]
async fn write_terminal_reports_whether_the_host_wrote() {
    let (tui, _ui) = native(Vec::new()).await;
    tui.session.prompt("/native-write", Vec::new()).await.unwrap();
    assert_eq!(entry_data(&tui, "native-write"), Some(json!({"wrote": true})));

    let print = native_with(Vec::new(), ExtensionMode::Print, Arc::new(NoOpExtensionUi), false).await;
    print.session.prompt("/native-write", Vec::new()).await.unwrap();
    assert_eq!(entry_data(&print, "native-write"), Some(json!({"wrote": false})));
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_without_a_ctx_are_answered_during_initialize() {
    let (h, _ui) = native(Vec::new()).await;
    h.session.prompt("/native-probe", Vec::new()).await.unwrap();
    let probe = entry_data(&h, "native-probe").unwrap();
    assert_eq!(probe["flag"], json!({"result": null}), "{probe}");
    let commands = probe["commands"]["error"].as_str().unwrap_or_default();
    assert!(commands.contains("cannot be called during extension loading"), "{probe}");
}
