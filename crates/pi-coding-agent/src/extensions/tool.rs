//! Tools registered by extensions (upstream `wrapRegisteredTool`): the agent sees an `AgentTool`
//! whose execution, argument preparation, and loadout hook run the extension's JavaScript.

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use pi_agent::{
    AgentTool, AgentToolResult, SharedTool, ToolAnnotations, ToolContext, ToolExecutionMode, ToolExposure, ToolLoadout,
    ToolLoadoutChanges, ToolNamespace, ToolReach, ToolUpdateCallback,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::registry::ToolDescriptor;
use super::runner::ExtensionRunner;
use crate::agent_session::lock;

/// How long synchronous hooks (`prepareArguments`, `prepareLoadout`) may take.
const HOOK_TIMEOUT: Duration = Duration::from_secs(2);

pub struct ExtensionTool {
    runner: Weak<ExtensionRunner>,
    descriptor: ToolDescriptor,
    exposure: ToolExposure,
    namespace: Option<ToolNamespace>,
    annotations: Option<ToolAnnotations>,
    guidelines: Vec<String>,
    /// An SDK `customTools` entry: its key in the host and the session target its calls see.
    sdk: Option<(u64, u64)>,
    /// The native tool of a factory tool (`descriptor.native`), whose hooks run in Rust.
    native: Option<SharedTool>,
}

impl ExtensionTool {
    pub fn new(runner: &Arc<ExtensionRunner>, descriptor: ToolDescriptor) -> Self {
        fn parse<T: serde::de::DeserializeOwned>(value: &Option<Value>) -> Option<T> {
            value.clone().and_then(|value| serde_json::from_value(value).ok())
        }
        let native = descriptor
            .native
            .as_ref()
            .and_then(|native| runner.session()?.factory_tool(native["name"].as_str()?, &native["options"], false));
        Self {
            native,
            runner: Arc::downgrade(runner),
            exposure: parse(&descriptor.exposure.clone().map(Value::from)).unwrap_or_default(),
            namespace: parse(&descriptor.namespace),
            annotations: parse(&descriptor.annotations),
            guidelines: descriptor.prompt_guidelines.clone().unwrap_or_default(),
            descriptor,
            sdk: None,
        }
    }

    /// An SDK custom tool whose definition is entry `key` of `runner`'s host, for session `target`.
    pub(crate) fn sdk(runner: &Arc<ExtensionRunner>, descriptor: ToolDescriptor, key: u64, target: u64) -> Self {
        Self { sdk: Some((key, target)), native: None, ..Self::new(runner, descriptor) }
    }

    pub fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    /// The host method and arguments of a tool call: by tool name, or by SDK key and target.
    fn host_call<'a>(&self, method: &'a str, args: Vec<Value>) -> (&'a str, Vec<Value>) {
        match self.sdk {
            Some((key, target)) => {
                ("sdkTool", [json!(key), json!(target), json!(method)].into_iter().chain(args).collect())
            }
            None => (method, std::iter::once(json!(self.descriptor.name)).chain(args).collect()),
        }
    }

    fn blocking_call(&self, runner: &ExtensionRunner, method: &str, args: Vec<Value>) -> Result<Value, pi_js::JsError> {
        let (method, args) = self.host_call(method, args);
        let args = args.into_iter().map(Into::into).collect();
        runner.host().blocking_call_method_within(method, args, HOOK_TIMEOUT).map(pi_js::HostValue::into_json)
    }

    fn blocking_hook(&self, method: &str, args: Vec<Value>) -> Option<Value> {
        let runner = self.runner.upgrade()?;
        match self.blocking_call(&runner, method, args) {
            Ok(value) => Some(value),
            Err(error) => {
                runner.emit_error(super::runner::ExtensionError {
                    extension_path: self.descriptor.extension_path.clone(),
                    event: method.into(),
                    error: error.message,
                    stack: error.stack,
                });
                None
            }
        }
    }
}

/// A `reachesTools` result: `"all"`, or the words of the input that may name tools.
fn reach_from_json(value: &Value) -> ToolReach {
    match value {
        Value::String(all) if all == "all" => ToolReach::All,
        Value::Array(words) => ToolReach::Words(words.iter().filter_map(Value::as_str).map(str::to_string).collect()),
        _ => ToolReach::Words(Vec::new()),
    }
}

fn tool_names(tools: &[pi_agent::SharedTool]) -> Value {
    tools
        .iter()
        .map(|tool| json!({"name": tool.name(), "label": tool.label(), "description": tool.description()}))
        .collect()
}

/// The loadout as `prepareLoadout` sees it: tool summaries plus per-tool exposure, namespace,
/// and guidelines, which the host turns back into upstream's accessor methods.
fn loadout_json(loadout: &ToolLoadout) -> Value {
    let all = loadout.registered.iter().chain(&loadout.declared).chain(&loadout.callable);
    let mut exposures = Map::new();
    let mut namespaces = Map::new();
    let mut guidelines = Map::new();
    for tool in all {
        let name = tool.name().to_string();
        exposures.insert(name.clone(), json!(loadout.exposure(&name)));
        if let Some(namespace) = loadout.namespace(&name) {
            namespaces.insert(name.clone(), json!(namespace));
        }
        guidelines.insert(name.clone(), json!(loadout.prompt_guidelines(&name)));
    }
    json!({
        "declared": tool_names(&loadout.declared),
        "callable": tool_names(&loadout.callable),
        "registered": tool_names(&loadout.registered),
        "exposures": exposures,
        "namespaces": namespaces,
        "guidelines": guidelines,
    })
}

#[async_trait]
impl AgentTool for ExtensionTool {
    fn name(&self) -> &str {
        &self.descriptor.name
    }

    fn label(&self) -> &str {
        &self.descriptor.label
    }

    fn description(&self) -> &str {
        &self.descriptor.description
    }

    fn parameters(&self) -> Value {
        self.descriptor.parameters.clone()
    }

    fn output_schema(&self) -> Option<Value> {
        self.descriptor.output_schema.clone()
    }

    fn constrained_sampling(&self) -> Option<Value> {
        self.descriptor.constrained_sampling.clone().filter(|value| value != &Value::Bool(false))
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn namespace(&self) -> Option<&ToolNamespace> {
        self.namespace.as_ref()
    }

    fn annotations(&self) -> Option<&ToolAnnotations> {
        self.annotations.as_ref()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        self.descriptor.prompt_snippet.as_deref()
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        self.guidelines.clone()
    }

    fn default_active(&self) -> bool {
        self.descriptor.default_active.unwrap_or(true)
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        match self.descriptor.execution_mode.as_deref() {
            Some("sequential") => Some(ToolExecutionMode::Sequential),
            Some("parallel") => Some(ToolExecutionMode::Parallel),
            _ => None,
        }
    }

    fn reaches_tools(&self, input: &Value) -> Option<ToolReach> {
        if !self.descriptor.has_reaches_tools {
            return self.native.as_ref().and_then(|native| native.reaches_tools(input));
        }
        // A handler that throws reaches nothing; the error goes to the extension error listeners.
        let reach = self.blocking_hook("reachesTools", vec![input.clone()]);
        Some(reach.as_ref().map_or_else(|| ToolReach::Words(Vec::new()), reach_from_json))
    }

    fn declares_reaches_tools(&self) -> bool {
        self.descriptor.has_reaches_tools || self.native.as_ref().is_some_and(|native| native.declares_reaches_tools())
    }

    fn prepare_arguments(&self, args: Value) -> anyhow::Result<Value> {
        if !self.descriptor.has_prepare_arguments {
            return Ok(args);
        }
        let runner = self.runner.upgrade().ok_or_else(|| anyhow::anyhow!("The extension host has shut down"))?;
        self.blocking_call(&runner, "prepareArguments", vec![args]).map_err(|error| anyhow::anyhow!(error.message))
    }

    fn prepare_loadout(&self, loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        if !self.descriptor.has_prepare_loadout {
            return self.native.as_ref().and_then(|native| native.prepare_loadout(loadout));
        }
        let changes = self.blocking_hook("prepareLoadout", vec![loadout_json(loadout)])?;
        if changes.is_null() {
            return None;
        }
        let descriptions = changes["descriptions"]
            .as_object()
            .map(|map| map.iter().filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string()))).collect())
            .unwrap_or_default();
        let hidden = changes["hiddenDeclarations"].as_array().cloned().unwrap_or_default();
        let hidden_declarations = hidden.iter().filter_map(|name| name.as_str().map(str::to_string)).collect();
        Some(ToolLoadoutChanges { descriptions, hidden_declarations })
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.execute_with_context(&ToolContext::new(tool_call_id), args, cancel, on_update).await
    }

    async fn execute_with_context(
        &self,
        context: &ToolContext,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let runner = self.runner.upgrade().ok_or_else(|| anyhow::anyhow!("The extension host has shut down"))?;
        let call_id = context.tool_call_id().to_string();
        lock(&runner.nested).insert(call_id.clone(), context.clone());
        let update_key = runner.next_id();
        lock(&runner.updates).insert(update_key, on_update);
        let signal = runner.signal_for(&cancel);
        let (method, call) =
            self.host_call("executeTool", vec![json!(call_id), args, json!(signal), json!(update_key)]);
        let result = runner.call_json(method, call).await;
        lock(&runner.nested).remove(&call_id);
        lock(&runner.updates).remove(&update_key);
        runner.release_signal(signal);
        let value = result.map_err(|error| anyhow::anyhow!(error.message))?;
        serde_json::from_value(value).map_err(|error| anyhow::anyhow!("Invalid tool result: {error}"))
    }
}
