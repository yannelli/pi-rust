//! Native extensions: the bridge starts `tests/fixtures/native/fixture.mjs` as a child process
//! (the pi binary in its Node-style mode) and talks the native protocol to it over stdio.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::extension_ui::{
    AutocompleteRequest, DialogOptions, ExtensionMode, ExtensionUi, NoOpExtensionUi, NotifyType, WidgetPlacement,
};
use pi_coding_agent::extensions::autocomplete::JsAutocompleteProvider;
use pi_coding_agent::extensions::{ComponentFactory, CustomRequest, ExtensionBindings};
use pi_coding_agent::model_resolver::ScopedModel;
use pi_tui::autocomplete::{AutocompleteProvider, AutocompleteSuggestions, SuggestionOptions};
use pi_tui::{AutocompleteItem, Component};
use serde_json::{Value, json};

use crate::harness::{Harness, Setup, eventually, harness};
use crate::http_mock::{Reply, Server};

#[derive(Default)]
pub(crate) struct NativeUi {
    calls: Mutex<Vec<String>>,
    answers: Mutex<Vec<String>>,
    widgets: Mutex<Vec<(String, ComponentFactory)>>,
    git_branch: Mutex<Value>,
    footer_reads: std::sync::atomic::AtomicUsize,
    autocomplete: Mutex<Vec<ComponentFactory>>,
    rows: Mutex<Option<u16>>,
}

impl NativeUi {
    fn push(&self, line: String) {
        self.calls.lock().unwrap().push(line);
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn widget(&self, key: &str) -> Option<ComponentFactory> {
        let widgets = self.widgets.lock().unwrap();
        widgets.iter().rev().find(|(name, _)| name == key).map(|(_, factory)| factory.clone())
    }
}

/// Render `component` until a line contains `text`.
pub(crate) fn render_until(component: &mut dyn Component, width: usize, text: &str) -> Vec<String> {
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

    async fn input(&self, title: &str, placeholder: Option<&str>, opts: DialogOptions) -> Option<String> {
        let prefill = opts.prefill.unwrap_or_default();
        self.push(format!("input: {title} {placeholder:?} prefill={prefill} secret={}", opts.secret));
        Some(format!("{prefill}c"))
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

    fn terminal_size(&self) -> (u16, u16) {
        (77, self.rows.lock().unwrap().unwrap_or(24))
    }

    fn footer_data(&self, kind: &str) -> Value {
        self.footer_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if kind == "gitBranch" { self.git_branch.lock().unwrap().clone() } else { Value::Null }
    }

    fn add_autocomplete_provider(&self, factory: ComponentFactory) {
        self.autocomplete.lock().unwrap().push(factory);
    }

    async fn autocomplete_base(&self, request: AutocompleteRequest) -> Option<AutocompleteSuggestions> {
        let before = &request.lines[request.cursor_line][..request.cursor_col];
        let prefix = before.rsplit(' ').next().unwrap_or_default().to_string();
        let items = ["@native-two", "@pi-own"].map(|value| AutocompleteItem::new(value, value)).to_vec();
        Some(AutocompleteSuggestions { items, prefix })
    }

    async fn login(&self, provider: &str) -> Result<bool, String> {
        self.push(format!("login: {provider}"));
        Ok(true)
    }

    async fn custom(&self, request: CustomRequest) -> Result<Option<Value>, String> {
        let mut component = request.factory.create_async().await?;
        let lines = tokio::task::block_in_place(|| {
            let first = component.render(30);
            let resized = render_until(&mut component, 30, "width 30");
            component.handle_input("\x1b[A");
            [first, resized].concat()
        });
        self.push(format!("panel: {}", lines.join("|")));
        Ok(Some(request.done.await.map_err(|error| error.to_string())?))
    }
}

pub(crate) fn package_json(env: &[(&str, &str)]) -> String {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native/fixture.mjs");
    let mut vars = json!({"PI_JS_FORK": "1"});
    for (name, value) in env {
        vars[*name] = json!(value);
    }
    json!({"name": "native-fixture", "pi": {"native": [{
        "command": env!("CARGO_BIN_EXE_pi"),
        "args": [fixture.display().to_string()],
        "env": vars,
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
    native_in(Setup { responses, ..Setup::default() }, &[], mode, ui, observer).await
}

/// The fixture loaded into `setup`, with `env` set for the child process.
pub(crate) async fn native_in(
    setup: Setup,
    env: &[(&str, &str)],
    mode: ExtensionMode,
    ui: Arc<dyn ExtensionUi>,
    observer: bool,
) -> Harness {
    let mut files = vec![("native-pkg/package.json", package_json(env))];
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
        bind: false,
        mode,
        ..setup
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

pub(crate) async fn native(responses: Vec<FauxResponse>) -> (Harness, Arc<NativeUi>) {
    let ui = Arc::new(NativeUi::default());
    (native_with(responses, ExtensionMode::Tui, ui.clone(), false).await, ui)
}

pub(crate) fn entry_data(h: &Harness, custom_type: &str) -> Option<Value> {
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
    assert_eq!(widget, [vec!["widget for tui".to_string(), "flag=on".into()], vec!["width 40 height 24".into()]]);

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
async fn ui_resize_sends_the_height_and_reports_height_changes_alone() {
    let (h, ui) = native(Vec::new()).await;
    eventually(|| ui.widget("native").is_some()).await;
    let factory = ui.widget("native").unwrap();
    let set_rows = |rows| *ui.rows.lock().unwrap() = Some(rows);
    let lines = tokio::task::block_in_place(|| {
        let mut component = factory.create().unwrap();
        let tall = render_until(&mut component, 40, "width 40 height 24");
        set_rows(12);
        let short = render_until(&mut component, 40, "width 40 height 12");
        set_rows(0);
        let unknown = render_until(&mut component, 40, "width 40 height none");
        let wider = render_until(&mut component, 50, "width 50 height none");
        [tall, short, unknown, wider].concat()
    });
    let expected = ["width 40 height 24", "width 40 height 12", "width 40 height none", "width 50 height none"];
    assert_eq!(lines, expected);

    set_rows(12);
    h.session.prompt("/native-ctx", Vec::new()).await.unwrap();
    assert_eq!(entry_data(&h, "native-ctx").unwrap()["terminalHeight"], json!(12));
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
    assert_eq!(data("native-panel"), Some(json!({"result": "closed by \u{1b}[A/up"})));
    assert_eq!(data("native-llm"), Some(json!({"text": "llm answer"})));
    let calls = ui.calls();
    assert!(calls.contains(&"select: Pick one [red, blue]".to_string()), "{calls:?}");
    assert!(calls.contains(&"panel: Panel one|width 30 height 24".to_string()), "{calls:?}");
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
async fn stderr_lines_become_notifications_in_the_tui() {
    let (h, ui) = native(Vec::new()).await;
    h.session.prompt("/native-log", Vec::new()).await.unwrap();
    let path = "[native:native-fixture/fixture.mjs]";
    let expected = [
        format!("notify(warning): {path} careful"),
        format!("notify(error): {path} broke"),
        format!("notify(warning): {path} plain line"),
    ];
    eventually(|| expected.iter().all(|line| ui.calls().contains(line))).await;
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
    assert_eq!((&probe["mode"], &probe["hasUI"]), (&json!("tui"), &json!(true)), "{probe}");
    let commands = probe["commands"]["error"].as_str().unwrap_or_default();
    assert!(commands.contains("cannot be called during extension loading"), "{probe}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ctx_snapshot_has_scoped_models_model_endpoint_and_terminal_size() {
    let (h, _ui) = native(Vec::new()).await;
    let runtime = h.session.model_runtime().clone();
    let other = runtime.get_model("local", "other").unwrap();
    let thinking = pi_ai::ThinkingLevel::High;
    h.session.set_scoped_models(vec![ScopedModel { model: other, thinking_level: Some(thinking) }]);
    h.session.prompt("/native-ctx", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-ctx").unwrap();
    assert_eq!(data["scopedModels"], json!([{"provider": "local", "id": "other", "thinkingLevel": "high"}]));
    assert_eq!(data["terminalWidth"], json!(77));
    assert_eq!(data["terminalHeight"], json!(24));
    assert_eq!(data["baseUrl"], json!("http://unused"));
    assert_eq!(data["compat"], Value::Null);
    let keys = data["keys"].to_string();
    assert!(!keys.contains("headers") && !keys.contains("apiKey"), "{keys}");

    let print = native_with(Vec::new(), ExtensionMode::Print, Arc::new(NoOpExtensionUi), false).await;
    print.session.prompt("/native-ctx", Vec::new()).await.unwrap();
    let data = entry_data(&print, "native-ctx").unwrap();
    assert_eq!(data["terminalWidth"], Value::Null);
    assert_eq!(data["terminalHeight"], Value::Null);
    assert_eq!(data["scopedModels"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn model_find_returns_the_snapshot_model_shape() {
    let (h, _ui) = native(Vec::new()).await;
    h.session.prompt("/native-find", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-find").unwrap();
    assert_eq!(data["found"]["id"], json!("other"));
    assert_eq!(data["found"]["provider"], json!("local"));
    assert_eq!(data["found"]["baseUrl"], json!("http://unused"));
    assert!(data["found"].get("headers").is_none() && data["found"].get("apiKey").is_none(), "{data}");
    assert_eq!(data["missing"], Value::Null);
    assert_eq!(data["noId"], json!({"result": null}), "{data}");
    assert_eq!(data["noProvider"], json!({"result": null}), "{data}");
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_get_reports_each_provider_with_an_error_state() {
    let (h, _ui) = native(Vec::new()).await;
    h.session.prompt("/native-usage", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-usage").unwrap();
    let cursor = &data["one"]["providers"][0];
    assert_eq!(cursor["provider"], json!("cursor"));
    assert_eq!(cursor["error"], json!("Not logged in. Run /login cursor."), "{data}");
    assert_eq!(cursor["windows"], json!([]));
    assert!(cursor["fetchedAt"].as_i64().is_some_and(|at| at > 0));
    assert_eq!(data["all"], json!({"providers": []}));
}

#[tokio::test(flavor = "multi_thread")]
async fn input_passes_prefill_and_secret() {
    let (h, ui) = native(Vec::new()).await;
    h.session.prompt("/native-input", Vec::new()).await.unwrap();
    assert_eq!(entry_data(&h, "native-input"), Some(json!({"value": "abc"})));
    assert!(ui.calls().contains(&r#"input: Token Some("tok") prefill=ab secret=true"#.to_string()), "{:?}", ui.calls());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn exec_writes_stdin_to_the_process() {
    let (h, _ui) = native(Vec::new()).await;
    h.session.prompt("/native-exec", Vec::new()).await.unwrap();
    assert_eq!(entry_data(&h, "native-exec"), Some(json!({"stdout": "piped text", "code": 0})));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_before_settle_results_append_entries() {
    let (h, _ui) = native(vec![FauxResponse::text("settled")]).await;
    h.session.prompt("/native-settle", Vec::new()).await.unwrap();
    h.session.prompt("settle now", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-settle").unwrap();
    assert_eq!(data["before"], json!(0), "{data}");
    assert!(data["outcome"].is_string(), "{data}");
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

#[tokio::test(flavor = "multi_thread")]
async fn footer_changes_reach_footer_changed_subscribers() {
    let (h, ui) = native(Vec::new()).await;
    eventually(|| ui.widget("native").is_some()).await;
    *ui.git_branch.lock().unwrap() = json!("feature-x");
    eventually(|| entry_data(&h, "native-footer").is_some()).await;
    assert_eq!(entry_data(&h, "native-footer").unwrap()["gitBranch"], json!("feature-x"));
}

#[tokio::test(flavor = "multi_thread")]
async fn merged_autocomplete_adds_pi_suggestions_after_the_extension_items() {
    let (_h, ui) = native(Vec::new()).await;
    eventually(|| !ui.autocomplete.lock().unwrap().is_empty()).await;
    let factories = ui.autocomplete.lock().unwrap().clone();
    let provider = tokio::task::block_in_place(|| JsAutocompleteProvider::build(&factories)).unwrap();
    let lines = vec!["see @na".to_string()];
    let suggestions = provider.get_suggestions(&lines, 0, 7, SuggestionOptions::default()).await.unwrap();
    let values: Vec<&str> = suggestions.items.iter().map(|item| item.value.as_str()).collect();
    assert_eq!(values, ["@native-one", "@native-two", "@pi-own"]);
    assert_eq!(suggestions.prefix, "@na");
}

fn footer_reads(ui: &NativeUi) -> usize {
    ui.footer_reads.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether `footer_data` is read within 700 ms (the poll reads it every 250 ms).
async fn footer_polled(ui: &NativeUi) -> bool {
    let before = footer_reads(ui);
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    footer_reads(ui) != before
}

#[tokio::test(flavor = "multi_thread")]
async fn the_footer_poll_needs_a_subscriber_and_stops_on_shutdown() {
    let ui = Arc::new(NativeUi::default());
    let setup = Setup::default();
    let h = native_in(setup, &[("NATIVE_FOOTER", "off")], ExtensionMode::Tui, ui.clone(), false).await;
    eventually(|| ui.widget("native").is_some()).await;
    assert!(!footer_polled(&ui).await, "the footer poll ran without a footer_changed subscriber");
    drop(h);

    let ui = Arc::new(NativeUi::default());
    let lingering = [("NATIVE_EXIT_DELAY_MS", "1500")];
    let h = native_in(Setup::default(), &lingering, ExtensionMode::Tui, ui.clone(), false).await;
    eventually(|| ui.widget("native").is_some()).await;
    assert!(footer_polled(&ui).await, "the footer poll did not run for a subscriber");
    h.runner.emit(json!({"type": "session_shutdown", "reason": "quit"})).await;
    let runner = h.runner.clone();
    let late = tokio::spawn(async move { runner.emit(json!({"type": "session_start", "reason": "startup"})).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!footer_polled(&ui).await, "the footer poll ran after shutdown");
    late.await.unwrap();
}

const STREAM: &str = concat!(
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"mock answer\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test(flavor = "multi_thread")]
async fn llm_complete_headers_replace_pi_headers_without_case() {
    let server =
        Server::start(|_| Reply { status: 200, content_type: "text/event-stream", body: STREAM.as_bytes().to_vec() })
            .await;
    let models = json!({"providers": {"mock": {"baseUrl": format!("{}/v1", server.url), "api": "openai-completions",
        "apiKey": "test-key", "headers": {"x-shared": "from pi", "x-pi": "pi"},
        "models": [{"id": "m1", "contextWindow": 1000, "maxTokens": 100}]}}});
    let setup = Setup {
        models: Some(models),
        settings: json!({"defaultProvider": "mock", "defaultModel": "m1"}),
        ..Setup::default()
    };
    let ui = Arc::new(NativeUi::default());
    let h = native_in(setup, &[], ExtensionMode::Tui, ui, false).await;
    h.session.prompt("/native-llm-headers", Vec::new()).await.unwrap();
    let data = entry_data(&h, "native-llm-headers").unwrap();
    assert_eq!(data["text"], json!("mock answer"), "{data}");
    for key in ["invalid", "list"] {
        let error = data[key]["error"].as_str().unwrap_or_default();
        assert!(error.contains("headers must be an object of string values"), "{data}");
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "invalid headers reached the server");
    let request = &requests[0];
    assert_eq!(request.header("x-shared"), Some("from extension"));
    assert_eq!(request.header("authorization"), Some("Bearer extension"));
    assert_eq!(request.header("x-pi"), Some("pi"));
    assert_eq!(request.header("x-extra"), Some("1"));
}
