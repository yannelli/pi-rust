//! MCP tools as pi agent tools. Results become model content (see `output`); codemode scripts get the
//! whole `CallToolResult` from [`McpTool::call`], whose output schema every MCP tool declares.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::client::RequestOptions;
use crate::config::McpExposure;
use crate::content::CallToolResult;
use crate::error::McpError;
use crate::output::{ConvertOptions, McpToolDetails, McpToolOutput, OutputSaver, convert_mcp_result, default_saver};
use crate::runtime::McpServerConnection;
use crate::types::Tool;

pub use pi_agent::{ToolAnnotations, ToolExposure, ToolNamespace};

/// `codemode` and `deferred` both leave tools out of the codemode description; they differ only in
/// which tool the integration activates to reach them.
pub fn to_tool_exposure(exposure: McpExposure) -> ToolExposure {
    match exposure {
        McpExposure::Codemode | McpExposure::Deferred => ToolExposure::Deferred,
        McpExposure::Direct => ToolExposure::Direct,
        McpExposure::Hidden => ToolExposure::Hidden,
    }
}

/// Calls a server's tools; implemented by server connections.
#[async_trait]
pub trait McpToolCaller: Send + Sync {
    async fn call_tool(&self, name: &str, args: Value, options: RequestOptions) -> Result<CallToolResult, McpError>;
}

#[async_trait]
impl McpToolCaller for McpServerConnection {
    async fn call_tool(&self, name: &str, args: Value, options: RequestOptions) -> Result<CallToolResult, McpError> {
        McpServerConnection::call_tool(self, name, args, options).await
    }
}

/// Resolves the caller when a call runs, so a call outlives a reconnect or reports a disabled server.
pub type CallerFn = Arc<dyn Fn() -> Result<Arc<dyn McpToolCaller>, String> + Send + Sync>;
pub type FlagFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// MCP content blocks as one compact type with the fields of text and image blocks, which scripts
/// read most (`image(block)` takes an image block as is). Rendered into every MCP tool's declaration.
fn content_block_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": { "enum": ["text", "image", "audio", "resource_link", "resource"] },
            "text": { "type": "string" },
            "data": { "type": "string" },
            "mimeType": { "type": "string" },
        },
        "required": ["type"],
        "additionalProperties": true,
    })
}

/// Keywords whose values are data, so a `$ref` key in them is not a reference.
const SCHEMA_DATA_KEYWORDS: [&str; 4] = ["const", "enum", "default", "examples"];
/// Keywords whose values map names to schemas, so their keys are names, not keywords.
const SCHEMA_MAP_KEYWORDS: [&str; 6] =
    ["properties", "patternProperties", "dependentSchemas", "dependencies", "$defs", "definitions"];

/// `schema` as a subschema at the JSON pointer `pointer` (`#/properties/structuredContent`) of another
/// schema: its local references (`#`, `#/$defs/Item`) are rewritten to point below `pointer`, so they
/// resolve to the same subschemas as before.
fn relocate_schema(schema: &Value, pointer: &str) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(|item| relocate_schema(item, pointer)).collect()),
        Value::Object(fields) => {
            let relocated = fields.iter().map(|(key, value)| {
                let value = match value {
                    Value::String(reference) if key == "$ref" && (reference == "#" || reference.starts_with("#/")) => {
                        Value::String(format!("{pointer}{}", &reference[1..]))
                    }
                    _ if SCHEMA_DATA_KEYWORDS.contains(&key.as_str()) => value.clone(),
                    Value::Object(named) if SCHEMA_MAP_KEYWORDS.contains(&key.as_str()) => Value::Object(
                        named
                            .iter()
                            .map(|(name, subschema)| (name.clone(), relocate_schema(subschema, pointer)))
                            .collect(),
                    ),
                    _ => relocate_schema(value, pointer),
                };
                (key.clone(), value)
            });
            Value::Object(relocated.collect())
        }
        other => other.clone(),
    }
}

/// Output schema of every MCP tool: the `CallToolResult` scripts receive (without `_meta`, which
/// [`convert_mcp_result`] drops), with the tool's own output schema as `structuredContent`.
pub fn create_mcp_result_schema(structured_content_schema: Option<&Value>) -> Value {
    // Servers may send structured content without declaring a schema for it.
    let structured = structured_content_schema.map_or_else(
        || json!({ "type": "object" }),
        |schema| relocate_schema(schema, "#/properties/structuredContent"),
    );
    json!({
        "type": "object",
        "properties": {
            "content": { "type": "array", "items": content_block_schema() },
            "structuredContent": structured,
            "isError": { "type": "boolean" },
        },
        "required": ["content"],
        // Fields newer protocol versions add.
        "additionalProperties": true,
    })
}

/// Input schemas must be objects. Servers may omit `type`, and some providers reject object schemas
/// without `properties`.
fn to_parameters(schema: &Value) -> Value {
    let mut schema = schema.as_object().cloned().unwrap_or_default();
    if schema.get("type").is_none_or(Value::is_null) {
        schema.insert("type".into(), json!("object"));
    }
    if !schema.contains_key("properties") {
        schema.insert("properties".into(), json!({}));
    }
    Value::Object(schema)
}

/// The boolean hints of a tool's annotations, or `None` without any.
fn to_tool_annotations(tool: &Tool) -> Option<ToolAnnotations> {
    let hint = |name: &str| tool.annotations.as_ref()?.get(name)?.as_bool();
    let annotations = ToolAnnotations {
        read_only_hint: hint("readOnlyHint"),
        destructive_hint: hint("destructiveHint"),
        idempotent_hint: hint("idempotentHint"),
        open_world_hint: hint("openWorldHint"),
    };
    (annotations != ToolAnnotations::default()).then_some(annotations)
}

pub struct McpToolOptions {
    pub server: String,
    pub tool: Tool,
    /// The pi tool name, from `create_mcp_tool_name`.
    pub name: String,
    pub exposure: McpExposure,
    pub namespace: ToolNamespace,
    pub timeout: Duration,
    pub caller: CallerFn,
    /// Whether `read_mcp_resource` can read the server's resources.
    pub readable_resources: Option<FlagFn>,
}

/// One MCP tool of one server.
#[derive(Clone)]
pub struct McpTool {
    name: String,
    label: String,
    description: String,
    parameters: Value,
    output_schema: Value,
    exposure: ToolExposure,
    namespace: ToolNamespace,
    annotations: Option<ToolAnnotations>,
    server: String,
    tool: String,
    timeout: Duration,
    caller: CallerFn,
    readable_resources: Option<FlagFn>,
    saver: OutputSaver,
}

impl McpTool {
    pub fn new(options: McpToolOptions) -> Self {
        let McpToolOptions { server, tool, name, exposure, namespace, timeout, caller, readable_resources } = options;
        let description = tool.description.as_deref().map(str::trim).filter(|text| !text.is_empty());
        let description = description
            .or_else(|| tool.display_title())
            .map(str::to_string)
            .unwrap_or_else(|| format!("MCP tool {} from server {server}", tool.name));
        Self {
            name,
            label: format!("{server}/{}", tool.name),
            description,
            parameters: to_parameters(&tool.input_schema),
            output_schema: create_mcp_result_schema(tool.output_schema.as_ref()),
            exposure: to_tool_exposure(exposure),
            namespace,
            annotations: to_tool_annotations(&tool),
            tool: tool.name,
            server,
            timeout,
            caller,
            readable_resources,
            saver: default_saver(),
        }
    }

    /// The same tool with another exposure, for example `Hidden` once the server dropped it.
    pub fn with_exposure(&self, exposure: ToolExposure) -> Self {
        Self { exposure, ..self.clone() }
    }

    /// Where truncated output and binary resources are saved. Default: temp files.
    pub fn with_saver(self, saver: OutputSaver) -> Self {
        Self { saver, ..self }
    }

    pub fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    pub fn namespace(&self) -> &ToolNamespace {
        &self.namespace
    }

    pub fn annotations(&self) -> Option<&ToolAnnotations> {
        self.annotations.as_ref()
    }

    pub fn output_schema(&self) -> &Value {
        &self.output_schema
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    /// The tool's name on its server.
    pub fn mcp_name(&self) -> &str {
        &self.tool
    }

    /// Call the tool. Progress notifications become updates ("Progress 3/10" or the server's message).
    pub async fn call(
        &self,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateCallback>,
    ) -> anyhow::Result<McpToolOutput> {
        let caller = (self.caller)().map_err(anyhow::Error::msg)?;
        let (server, tool) = (self.server.clone(), self.tool.clone());
        let on_progress = on_update.map(|on_update| {
            Arc::new(move |progress: &crate::types::ProgressNotification| {
                let total = progress.total.map(|total| format!("/{}", number(total))).unwrap_or_default();
                let text = progress
                    .message
                    .clone()
                    .unwrap_or_else(|| format!("Progress {}{total}", number(progress.progress)));
                let details = json!({ "server": server, "tool": tool });
                on_update(AgentToolResult { details: Some(details), ..AgentToolResult::text(text) });
            }) as crate::client::ProgressFn
        });
        let options = RequestOptions { cancel: Some(cancel), timeout: Some(self.timeout), on_progress };
        let args = if args.is_null() { json!({}) } else { args };
        let result = caller.call_tool(&self.tool, args, options).await?;
        let readable_resources = self.readable_resources.as_ref().is_some_and(|readable| readable());
        let options = ConvertOptions { saver: self.saver.clone(), readable_resources };
        Ok(convert_mcp_result(&self.server, &self.tool, &result, &options))
    }
}

/// Numbers as JavaScript prints them: integers without a fraction.
pub(crate) fn number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 { format!("{}", value as i64) } else { value.to_string() }
}

/// The agent result of a converted MCP result.
pub fn to_agent_result(output: McpToolOutput) -> AgentToolResult {
    AgentToolResult {
        content: output.content,
        details: Some(details_json(&output.details)),
        structured_content: Some(output.structured_content),
        is_error: output.is_error,
        ..AgentToolResult::default()
    }
}

pub(crate) fn details_json(details: &McpToolDetails) -> Value {
    serde_json::to_value(details).unwrap_or(Value::Null)
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(self.output_schema.clone())
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn namespace(&self) -> Option<&ToolNamespace> {
        Some(&self.namespace)
    }

    fn annotations(&self) -> Option<&ToolAnnotations> {
        self.annotations.as_ref()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok(to_agent_result(self.call(args, cancel, Some(on_update)).await?))
    }
}
