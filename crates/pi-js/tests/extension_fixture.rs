mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::*;
use pi_js::{HostFunction, HostValue, JsRef, VirtualModule};
use serde_json::{Value, json};

const PI_CODING_AGENT: &str = "export const defineTool = (tool) => tool;";
const TYPEBOX: &str = r#"
export const Type = {
  Object: (properties, options = {}) => ({ type: "object", properties, required: Object.keys(properties), ...options }),
  String: (options = {}) => ({ type: "string", ...options }),
};
export default { Type };
"#;
const PI_TUI: &str = r#"
export class Text {
  constructor(text, paddingX = 1, paddingY = 1) { this.text = text; this.paddingX = paddingX; this.paddingY = paddingY; }
  render(width) { return [this.text.slice(0, width)]; }
  invalidate() {}
}
"#;

// The host half of the extension API, forwarding registrations to Rust.
const HOST_API: &str = r#"
import { call } from "pi:host";
export function createApi() {
  return {
    registerTool(tool) {
      const { name, label, description, parameters } = tool;
      call("registerTool", { name, label, description, parameters }, tool.execute);
    },
    registerCommand(name, options) { call("registerCommand", name, options.description, options.handler); },
    on(event, handler) { call("on", event, handler); },
    registerMessageRenderer(type, renderer) { call("registerRenderer", type, renderer); },
  };
}
export function createContext(cwd) {
  return { cwd, hasUI: true, ui: { notify: (message, level) => call("notify", message, level) } };
}
export const theme = { fg: (color, text) => `<${color}>${text}</${color}>` };
"#;

#[derive(Default)]
struct Recorded {
    tools: Vec<(Value, JsRef)>,
    commands: Vec<(String, String, JsRef)>,
    handlers: Vec<(String, JsRef)>,
    renderers: Vec<(String, JsRef)>,
    notifications: Vec<(String, String)>,
}

fn text(args: &[HostValue], index: usize) -> String {
    args.get(index).and_then(HostValue::as_str).unwrap_or_default().to_string()
}

fn reference(args: &[HostValue], index: usize) -> JsRef {
    args[index].as_js_ref().cloned().expect("a function argument")
}

fn register_host(runtime: &pi_js::JsRuntime, recorded: &Arc<Mutex<Recorded>>) {
    let record = recorded.clone();
    runtime.register_host_function(
        "registerTool",
        HostFunction::sync(move |args| {
            record.lock().unwrap().tools.push((args[0].clone().into_json(), reference(&args, 1)));
            Ok(HostValue::Undefined)
        }),
    );
    let record = recorded.clone();
    runtime.register_host_function(
        "registerCommand",
        HostFunction::sync(move |args| {
            record.lock().unwrap().commands.push((text(&args, 0), text(&args, 1), reference(&args, 2)));
            Ok(HostValue::Undefined)
        }),
    );
    let record = recorded.clone();
    runtime.register_host_function(
        "on",
        HostFunction::sync(move |args| {
            record.lock().unwrap().handlers.push((text(&args, 0), reference(&args, 1)));
            Ok(HostValue::Undefined)
        }),
    );
    let record = recorded.clone();
    runtime.register_host_function(
        "registerRenderer",
        HostFunction::sync(move |args| {
            record.lock().unwrap().renderers.push((text(&args, 0), reference(&args, 1)));
            Ok(HostValue::Undefined)
        }),
    );
    let record = recorded.clone();
    runtime.register_host_function(
        "notify",
        HostFunction::sync(move |args| {
            record.lock().unwrap().notifications.push((text(&args, 0), text(&args, 1)));
            Ok(HostValue::Undefined)
        }),
    );
}

#[tokio::test]
async fn loads_and_drives_a_pi_style_extension() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    runtime.register_virtual_module("@earendil-works/pi-coding-agent", VirtualModule::Source(PI_CODING_AGENT.into()));
    runtime.register_virtual_module("typebox", VirtualModule::Source(TYPEBOX.into()));
    runtime.register_virtual_module("@earendil-works/pi-tui", VirtualModule::Source(PI_TUI.into()));
    let recorded: Arc<Mutex<Recorded>> = Arc::default();
    register_host(&runtime, &recorded);

    let host = runtime.eval_module("pi-host-api.js", HOST_API).await.unwrap();
    let api = host.call_method_ref("createApi", vec![]).await.unwrap();
    let ctx = host.call_method_ref("createContext", vec![root.display().to_string().into()]).await.unwrap();
    let theme = host.get_ref("theme").await.unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/notes-extension/index.ts");
    let factory = runtime.import_default(fixture.display().to_string()).await.unwrap();
    factory.call(vec![HostValue::Ref(api)]).await.unwrap();

    let (tool, execute, command, handlers, renderer) = {
        let recorded = recorded.lock().unwrap();
        let handlers: Vec<(String, JsRef)> = recorded.handlers.clone();
        (
            recorded.tools[0].0.clone(),
            recorded.tools[0].1.clone(),
            recorded.commands[0].clone(),
            handlers,
            recorded.renderers[0].clone(),
        )
    };
    assert_eq!(
        tool,
        json!({
            "name": "write_note",
            "label": "Write note",
            "description": "Write a markdown note under .notes",
            "parameters": {
                "type": "object",
                "properties": { "title": { "type": "string", "description": "Note title" }, "body": { "type": "string" } },
                "required": ["title", "body"],
            },
        })
    );
    assert_eq!((command.0.as_str(), command.1.as_str()), ("notes", "List saved notes"));
    let events: Vec<&str> = handlers.iter().map(|(event, _)| event.as_str()).collect();
    assert_eq!(events, ["session_start", "tool_call"]);
    assert_eq!(renderer.0, "note");

    handlers[0].1.call(vec![json!({ "type": "session_start" }).into(), HostValue::Ref(ctx.clone())]).await.unwrap();
    let folder = root.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(recorded.lock().unwrap().notifications, vec![(format!("session 1 in {folder}"), "info".to_string())]);

    let blocked =
        handlers[1].1.call(vec![json!({ "toolName": "bash", "input": { "command": "rm -rf /" } }).into()]).await;
    assert_eq!(blocked.unwrap().into_json(), json!({ "block": true, "reason": "destructive command" }));
    let allowed = handlers[1].1.call(vec![json!({ "toolName": "bash", "input": { "command": "ls" } }).into()]).await;
    assert!(allowed.unwrap().is_undefined());

    let updates: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = updates.clone();
    let on_update = runtime
        .with(move |scope| {
            let function = scope.host_function(HostFunction::sync(move |args| {
                sink.lock().unwrap().push(args[0].clone().into_json());
                Ok(HostValue::Undefined)
            }))?;
            Ok(scope.persist(function.into_value()))
        })
        .await
        .unwrap();
    let params = json!({ "title": "Shopping List!", "body": "milk" });
    let args = vec![
        "call-1".into(),
        params.into(),
        HostValue::Undefined,
        HostValue::Ref(on_update),
        HostValue::Ref(ctx.clone()),
    ];
    let result = execute.call(args).await.unwrap().into_json();
    let file = format!(".notes{}shopping-list.md", std::path::MAIN_SEPARATOR);
    let written = std::fs::read_to_string(root.join(".notes/shopping-list.md")).unwrap();
    assert_eq!(written, "# Shopping List!\n\nmilk\n");
    assert_eq!(
        result,
        json!({ "content": [{ "type": "text", "text": format!("Saved {file}") }], "details": { "file": file, "bytes": written.len() } })
    );
    assert_eq!(
        *updates.lock().unwrap(),
        vec![json!({ "content": [{ "type": "text", "text": "writing Shopping List!" }] })]
    );

    command.2.call(vec!["".into(), HostValue::Ref(ctx.clone())]).await.unwrap();
    let last = recorded.lock().unwrap().notifications.last().cloned().unwrap();
    assert_eq!(last, ("shopping-list.md".to_string(), "info".to_string()));

    let component = renderer
        .1
        .call_ref(vec![json!({ "content": "hello" }).into(), json!({}).into(), HostValue::Ref(theme)])
        .await
        .unwrap();
    let lines = component.call_method("render", vec![json!(40).into()]).await.unwrap().into_json();
    assert_eq!(lines, json!(["<accent>hello</accent>"]));
    let rendered_elsewhere =
        std::thread::spawn(move || component.blocking_call_method("render", vec![json!(8).into()]).unwrap())
            .join()
            .unwrap();
    assert_eq!(rendered_elsewhere.into_json(), json!(["<accent>"]));
    assert!(capture.uncaught.lock().unwrap().is_empty());
}
