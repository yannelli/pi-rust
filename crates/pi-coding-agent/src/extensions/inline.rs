//! Extensions supplied by the host program (upstream SDK `InlineExtension`), written in Rust.
//! Each runs as an extension in load order: a generated JavaScript factory forwards its events
//! to [`InlineExtension::on_event`], and its tools register with the session.

use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::SharedTool;
use pi_js::JsError;
use serde_json::{Value, json};

use super::loader::LoadEntry;
use super::ops::Args;
use super::runner::ExtensionRunner;
use super::vendor::{specifier, vendor_root};
use crate::agent_session::{AgentSession, lock};
use crate::resources::source_info::BUILTIN_PATH_PREFIX;

/// An extension implemented in Rust.
#[async_trait]
pub trait InlineExtension: Send + Sync {
    /// Shown as `<inline:name>`, or `builtin:name` for a built-in extension.
    fn name(&self) -> &str;

    /// Left out of the startup Extensions list.
    fn hidden(&self) -> bool {
        false
    }

    /// Left out when another extension registers a tool or command of the same name.
    fn replaceable(&self) -> bool {
        false
    }

    /// Supplies the code of `builtin:<name>` instead of loading as an inline extension.
    fn builtin(&self) -> bool {
        false
    }

    /// Event types `on_event` handles, such as `tool_call` or `session_start`.
    fn events(&self) -> Vec<String> {
        Vec::new()
    }

    /// Handle one event in its upstream JSON form; the result is the handler's return value.
    async fn on_event(&self, _event: Value, _session: Option<Arc<AgentSession>>) -> Option<Value> {
        None
    }

    /// Tools this extension registers.
    fn tools(&self) -> Vec<SharedTool> {
        Vec::new()
    }
}

/// The extension path of an inline extension.
pub fn inline_path(extension: &dyn InlineExtension) -> String {
    if extension.builtin() {
        format!("{BUILTIN_PATH_PREFIX}{}", extension.name())
    } else {
        format!("<inline:{}>", extension.name())
    }
}

fn factory_source(index: usize, events: &[String]) -> String {
    let bridge = json!(specifier(&vendor_root(), "host/bridge.js"));
    format!(
        "import {{ callAsync, optional }} from {bridge};\n\
         export default function (pi) {{\n\
         \tfor (const type of {events}) pi.on(type, async (event) => optional(await callAsync(\"inline.event\", {index}, event)));\n\
         }}\n",
        events = json!(events)
    )
}

impl ExtensionRunner {
    /// Load inline extensions, then a later [`Self::load_entries`] loads the files.
    pub(crate) async fn load_inline(&self, extensions: &[Arc<dyn InlineExtension>]) {
        if extensions.is_empty() {
            return;
        }
        let taken: Vec<String> = extensions
            .iter()
            .filter(|extension| !extension.replaceable())
            .flat_map(|extension| extension.tools().iter().map(|tool| tool.name().to_string()).collect::<Vec<_>>())
            .collect();
        let mut entries = Vec::new();
        let mut files = Vec::new();
        for extension in extensions {
            if extension.replaceable() && extension.tools().iter().any(|tool| taken.contains(&tool.name().to_string())) {
                continue;
            }
            let index = {
                let mut inline = lock(&self.inline);
                inline.push(extension.clone());
                inline.len() - 1
            };
            let file = vendor_root().join("inline").join(format!("{index}.js"));
            files.push((file.clone(), factory_source(index, &extension.events())));
            let path = inline_path(extension.as_ref());
            entries.push(LoadEntry { path, resolved_path: file, source_info: None });
        }
        self.js().register_source_files(files);
        self.load_entries(&entries).await;
    }

    /// Tools of the inline extensions by extension path.
    pub(crate) fn inline_tools(&self) -> Vec<(String, Vec<SharedTool>)> {
        let inline = lock(&self.inline).clone();
        inline.iter().map(|extension| (inline_path(extension.as_ref()), extension.tools())).collect()
    }
}

/// `inline.event`: an event for the inline extension at an index.
pub(crate) async fn event(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let extension = lock(&runner.inline).get(args.u64(0) as usize).cloned();
    let Some(extension) = extension else { return Ok(Value::Null) };
    Ok(extension.on_event(args.json(1), runner.session()).await.unwrap_or(Value::Null))
}
