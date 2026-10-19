#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_ai::ContentBlock;
use pi_codemode::{
    CodemodeHost, CodemodeModels, CodemodeStoreEntryData, CodemodeToolInfo, CodemodeToolOptions, CodemodeToolOutput,
    NestedToolOutcome, execute_codemode, read_codemode_store, set_codemode_worker_limit,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

pub const TINY_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

type Handler = Arc<dyn Fn(Value) -> BoxFuture<'static, NestedToolOutcome> + Send + Sync>;

/// A session stand-in: tools with handlers, a branch of `codemode-store` entries, and models.
pub struct FakeHost {
    pub tools: Vec<CodemodeToolInfo>,
    handlers: HashMap<String, Handler>,
    calls: AtomicUsize,
    pub entries: Mutex<Vec<Value>>,
    pub models: Option<Box<dyn CodemodeModels>>,
}

impl Default for FakeHost {
    fn default() -> Self {
        // Tests run scripts in parallel; the worker limit has its own test binary.
        set_codemode_worker_limit(usize::MAX);
        Self {
            tools: Vec::new(),
            handlers: HashMap::new(),
            calls: AtomicUsize::new(0),
            entries: Mutex::new(Vec::new()),
            models: None,
        }
    }
}

pub fn text_outcome(text: &str) -> NestedToolOutcome {
    NestedToolOutcome { content: vec![ContentBlock::text(text)], ..NestedToolOutcome::default() }
}

pub fn error_outcome(text: &str) -> NestedToolOutcome {
    NestedToolOutcome { is_error: true, ..text_outcome(text) }
}

impl FakeHost {
    pub fn tool<F>(mut self, info: CodemodeToolInfo, handler: F) -> Self
    where
        F: Fn(Value) -> NestedToolOutcome + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        self.handlers.insert(
            info.name.clone(),
            Arc::new(move |args| {
                let outcome = handler(args);
                Box::pin(async move { outcome })
            }),
        );
        self.tools.push(info);
        self
    }

    pub fn async_tool<F>(mut self, info: CodemodeToolInfo, handler: F) -> Self
    where
        F: Fn(Value) -> BoxFuture<'static, NestedToolOutcome> + Send + Sync + 'static,
    {
        self.handlers.insert(info.name.clone(), Arc::new(handler));
        self.tools.push(info);
        self
    }

    /// The standard tools of the upstream suite: `echo`, `stats` (structured), and `screenshot`.
    pub fn standard() -> Self {
        let echo_schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string", "description": "Text to echo" } },
            "required": ["text"],
        });
        let stats = CodemodeToolInfo {
            output_schema: Some(json!({
                "type": "object",
                "properties": { "files": { "type": "number" }, "names": { "type": "array", "items": { "type": "string" } } },
                "required": ["files", "names"],
            })),
            ..CodemodeToolInfo::new("stats", "Return structured stats", json!({ "type": "object", "properties": {} }))
        };
        Self::default()
            .tool(CodemodeToolInfo::new("echo", "Echo text back.\n\nSecond paragraph.", echo_schema), |args| {
                text_outcome(&format!("echo: {}", args["text"].as_str().unwrap_or_default()))
            })
            .tool(stats, |_| NestedToolOutcome {
                structured_content: Some(json!({ "files": 2, "names": ["a", "b"] })),
                ..text_outcome("2 files")
            })
            .tool(CodemodeToolInfo::new("screenshot", "Return a screenshot", json!({ "type": "object" })), |_| {
                NestedToolOutcome {
                    content: vec![
                        ContentBlock::text("captured"),
                        ContentBlock::Image { data: TINY_PNG_BASE64.into(), mime_type: "image/png".into() },
                    ],
                    ..NestedToolOutcome::default()
                }
            })
    }

    pub fn with_models(mut self, models: Box<dyn CodemodeModels>) -> Self {
        self.models = Some(models);
        self
    }

    pub fn store_entries(&self) -> Vec<Value> {
        self.entries.lock().unwrap().clone()
    }
}

#[async_trait]
impl CodemodeHost for FakeHost {
    fn tools(&self) -> Vec<CodemodeToolInfo> {
        self.tools.clone()
    }

    async fn execute_tool(
        &self,
        parent: &str,
        name: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> NestedToolOutcome {
        let id = format!("{parent}/{}", self.calls.fetch_add(1, Ordering::SeqCst) + 1);
        let outcome = match self.handlers.get(name) {
            Some(handler) => handler(args).await,
            None => error_outcome(&format!("Tool {name} not found")),
        };
        NestedToolOutcome { tool_call_id: id, ..outcome }
    }

    fn store_snapshot(&self) -> Map<String, Value> {
        let entries = self.entries.lock().unwrap();
        read_codemode_store(entries.iter().map(|data| (pi_codemode::CODEMODE_STORE_ENTRY_TYPE, data)))
    }

    fn append_store_entry(&self, data: CodemodeStoreEntryData) {
        self.entries.lock().unwrap().push(serde_json::to_value(data).unwrap());
    }

    fn models(&self) -> Option<&dyn CodemodeModels> {
        self.models.as_deref()
    }
}

pub async fn run_script(host: &Arc<FakeHost>, code: &str) -> CodemodeToolOutput {
    run_with(host, code, &CodemodeToolOptions::default()).await
}

pub async fn run_with(host: &Arc<FakeHost>, code: &str, options: &CodemodeToolOptions) -> CodemodeToolOutput {
    let host: Arc<dyn CodemodeHost> = host.clone();
    execute_codemode(host, "call-1", code, CancellationToken::new(), None, options).await.expect("valid source")
}

/// The output after the script header, which is checked on the way. Images show as `<image>`.
pub fn result_text(output: &CodemodeToolOutput) -> String {
    let (header, items) = output.content.split_first().unwrap();
    let header = header.as_text().unwrap();
    let rest = header
        .strip_prefix("Script completed\nWall time ")
        .or_else(|| header.strip_prefix("Script failed\nWall time "))
        .unwrap_or_else(|| panic!("bad header {header:?}"));
    let seconds = rest.strip_suffix(" seconds\nOutput:\n").unwrap();
    let (whole, tenths) = seconds.split_once('.').unwrap();
    assert!(whole.chars().all(|c| c.is_ascii_digit()) && tenths.len() == 1, "{header:?}");
    items
        .iter()
        .map(|item| match item {
            ContentBlock::Text { text, .. } => text.clone(),
            _ => "<image>".into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace `[Image saved to ...]` labels with `<saved>` after checking each file holds the tiny
/// PNG, and remove the files.
pub fn check_saved_images(text: &str) -> String {
    use base64::Engine;
    text.split('\n')
        .map(|line| {
            let Some(rest) = line.strip_prefix("[Image saved to ") else { return line.to_string() };
            let (path, kind) = rest.split_once(" (").unwrap();
            assert!(path.ends_with(".png") && kind.starts_with("image/png, ") && kind.ends_with("B)]"), "{line}");
            let bytes = std::fs::read(path).unwrap();
            std::fs::remove_file(path).unwrap();
            assert_eq!(base64::engine::general_purpose::STANDARD.encode(bytes), TINY_PNG_BASE64);
            "<saved>".to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A registered tool built from its script view, for loadout tests. It never runs.
pub struct InfoTool {
    pub info: CodemodeToolInfo,
    pub exposure: pi_agent::ToolExposure,
}

impl InfoTool {
    pub fn shared(info: CodemodeToolInfo, exposure: pi_agent::ToolExposure) -> pi_agent::SharedTool {
        Arc::new(Self { info, exposure })
    }
}

#[async_trait]
impl pi_agent::AgentTool for InfoTool {
    fn name(&self) -> &str {
        &self.info.name
    }

    fn description(&self) -> &str {
        &self.info.description
    }

    fn parameters(&self) -> Value {
        self.info.parameters.clone()
    }

    fn output_schema(&self) -> Option<Value> {
        self.info.output_schema.clone()
    }

    fn exposure(&self) -> pi_agent::ToolExposure {
        self.exposure
    }

    fn namespace(&self) -> Option<&pi_agent::ToolNamespace> {
        self.info.namespace.as_ref()
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        self.info.prompt_guidelines.clone()
    }

    async fn execute(
        &self,
        _id: &str,
        _args: Value,
        _cancel: CancellationToken,
        _on_update: pi_agent::ToolUpdateCallback,
    ) -> anyhow::Result<pi_agent::AgentToolResult> {
        anyhow::bail!("{} is not executable in tests", self.info.name)
    }
}
