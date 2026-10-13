//! MCP resources through the tools Codex and opencode use: `list_mcp_resources`,
//! `list_mcp_resource_templates`, and `read_mcp_resource`. They take a `server` argument and cover
//! every connected server with resources. Listings are JSON, as in Codex, without MCP App resources.

use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::client::RequestOptions;
use crate::config::McpExposure;
use crate::content::ContentBlock;
use crate::error::McpError;
use crate::naming::{LIST_MCP_RESOURCE_TEMPLATES_TOOL, LIST_MCP_RESOURCES_TOOL, READ_MCP_RESOURCE_TOOL};
use crate::output::{
    ConvertOptions, McpToolDetails, McpToolOutput, OutputSaver, default_saver, limit_mcp_content, to_model_content,
};
use crate::runtime::McpServerConnection;
use crate::tools::{ToolAnnotations, ToolExposure, to_agent_result, to_tool_exposure};
use crate::types::{ListResourceTemplatesResult, ListResourcesResult, ReadResourceResult, Resource, ResourceTemplate};

mod schemas;

/// A connected server that offers resources.
#[async_trait]
pub trait McpResourceServer: Send + Sync {
    fn name(&self) -> String;
    fn timeout(&self) -> Duration;
    async fn resources_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourcesResult, McpError>;
    async fn resource_templates_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourceTemplatesResult, McpError>;
    async fn all_resources(&self, options: RequestOptions) -> Result<Vec<Resource>, McpError>;
    async fn all_resource_templates(&self, options: RequestOptions) -> Result<Vec<ResourceTemplate>, McpError>;
    async fn read_resource(&self, uri: &str, options: RequestOptions) -> Result<ReadResourceResult, McpError>;
}

#[async_trait]
impl McpResourceServer for McpServerConnection {
    fn name(&self) -> String {
        McpServerConnection::name(self).to_string()
    }

    fn timeout(&self) -> Duration {
        McpServerConnection::timeout(self)
    }

    async fn resources_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourcesResult, McpError> {
        McpServerConnection::resources_page(self, cursor, options).await
    }

    async fn resource_templates_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        McpServerConnection::resource_templates_page(self, cursor, options).await
    }

    async fn all_resources(&self, options: RequestOptions) -> Result<Vec<Resource>, McpError> {
        McpServerConnection::all_resources(self, options).await
    }

    async fn all_resource_templates(&self, options: RequestOptions) -> Result<Vec<ResourceTemplate>, McpError> {
        McpServerConnection::all_resource_templates(self, options).await
    }

    async fn read_resource(&self, uri: &str, options: RequestOptions) -> Result<ReadResourceResult, McpError> {
        McpServerConnection::read_resource(self, uri, options).await
    }
}

/// The servers the resource tools reach, asked for at call time.
pub type ResourceServersFn = Arc<dyn Fn() -> Vec<Arc<dyn McpResourceServer>> + Send + Sync>;

/// Order like JavaScript's `localeCompare` for plain names: case-insensitive, lowercase first.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Resources,
    Templates,
    Read,
}

const READ_ONLY: ToolAnnotations = ToolAnnotations {
    read_only_hint: Some(true),
    destructive_hint: None,
    idempotent_hint: None,
    open_world_hint: None,
};

/// One of the three resource tools.
#[derive(Clone)]
pub struct McpResourceTool {
    kind: Kind,
    exposure: ToolExposure,
    servers: ResourceServersFn,
    saver: OutputSaver,
}

/// The three resource tools with the exposure of the servers they reach.
pub fn create_mcp_resource_tools(exposure: McpExposure, servers: ResourceServersFn) -> Vec<McpResourceTool> {
    [Kind::Resources, Kind::Templates, Kind::Read]
        .into_iter()
        .map(|kind| McpResourceTool {
            kind,
            exposure: to_tool_exposure(exposure),
            servers: servers.clone(),
            saver: default_saver(),
        })
        .collect()
}

/// A string argument, trimmed; empty counts as absent.
fn string_argument(args: &Value, key: &str) -> anyhow::Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty())),
        Some(_) => anyhow::bail!("{key} must be a string"),
    }
}

/// A listed resource or template without `_meta` and icons, tagged with its server.
fn listed(server: &str, item: &impl Serialize) -> Value {
    let mut entry = Map::new();
    entry.insert("server".into(), json!(server));
    if let Ok(Value::Object(fields)) = serde_json::to_value(item) {
        entry.extend(fields.into_iter().filter(|(key, _)| key != "_meta" && key != "icons"));
    }
    Value::Object(entry)
}

fn listed_page<T: Serialize>(server: &str, key: &str, items: Vec<T>, next_cursor: Option<String>) -> Value {
    let mut payload = Map::new();
    payload.insert("server".into(), json!(server));
    payload.insert(key.into(), Value::Array(items.iter().map(|item| listed(server, item)).collect()));
    if let Some(cursor) = next_cursor {
        payload.insert("nextCursor".into(), json!(cursor));
    }
    Value::Object(payload)
}

impl McpResourceTool {
    pub fn with_saver(self, saver: OutputSaver) -> Self {
        Self { saver, ..self }
    }

    pub fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    pub fn annotations(&self) -> ToolAnnotations {
        READ_ONLY.clone()
    }

    pub fn output_schema(&self) -> Value {
        match self.kind {
            Kind::Resources => schemas::list_output(),
            Kind::Templates => schemas::list_templates_output(),
            Kind::Read => schemas::read_output(),
        }
    }

    fn find_server(&self, name: &str) -> anyhow::Result<Arc<dyn McpResourceServer>> {
        let servers = (self.servers)();
        if let Some(server) = servers.iter().find(|server| server.name() == name) {
            return Ok(server.clone());
        }
        let available: Vec<String> = servers.iter().map(|server| server.name()).collect();
        let available = if available.is_empty() {
            String::new()
        } else {
            format!(". Servers with resources: {}", available.join(", "))
        };
        anyhow::bail!("MCP server \"{name}\" has no resources{available}")
    }

    /// One page of one server, or every page of every server.
    async fn list(&self, args: &Value, cancel: &CancellationToken) -> anyhow::Result<Value> {
        let key = if self.kind == Kind::Resources { "resources" } else { "resourceTemplates" };
        let cursor = string_argument(args, "cursor")?;
        let options = |server: &dyn McpResourceServer| RequestOptions {
            cancel: Some(cancel.clone()),
            timeout: Some(server.timeout()),
            on_progress: None,
        };
        if let Some(name) = string_argument(args, "server")? {
            let server = self.find_server(&name)?;
            let (name, options) = (server.name(), options(server.as_ref()));
            return Ok(if self.kind == Kind::Resources {
                let page = server.resources_page(cursor.as_deref(), options).await?;
                let items = page.resources.into_iter().filter(|item| !item.is_mcp_app()).collect();
                listed_page(&name, key, items, page.next_cursor)
            } else {
                let page = server.resource_templates_page(cursor.as_deref(), options).await?;
                let items = page.resource_templates.into_iter().filter(|item| !item.is_mcp_app()).collect();
                listed_page(&name, key, items, page.next_cursor)
            });
        }
        if cursor.is_some() {
            anyhow::bail!("cursor can only be used when a server is specified");
        }
        let mut servers = (self.servers)();
        servers.sort_by(|a, b| locale_cmp(&a.name(), &b.name()));
        let results = futures::future::join_all(servers.iter().map(|server| {
            let options = options(server.as_ref());
            async move {
                let name = server.name();
                let items: Result<Vec<Value>, McpError> = if self.kind == Kind::Resources {
                    server.all_resources(options).await.map(|items| {
                        items.iter().filter(|item| !item.is_mcp_app()).map(|item| listed(&name, item)).collect()
                    })
                } else {
                    server.all_resource_templates(options).await.map(|items| {
                        items.iter().filter(|item| !item.is_mcp_app()).map(|item| listed(&name, item)).collect()
                    })
                };
                (name, items)
            }
        }))
        .await;
        let (mut items, mut errors) = (Vec::new(), Vec::new());
        for (server, result) in results {
            match result {
                Ok(listed) => items.extend(listed),
                Err(error) => errors.push(json!({ "server": server, "error": error.to_string() })),
            }
        }
        let mut payload = Map::new();
        payload.insert(key.into(), Value::Array(items));
        if !errors.is_empty() {
            payload.insert("errors".into(), Value::Array(errors));
        }
        Ok(Value::Object(payload))
    }

    fn json_result(&self, server: Option<String>, payload: Value) -> McpToolOutput {
        let text = vec![pi_ai::ContentBlock::text(payload.to_string())];
        let (content, full_output_path) = limit_mcp_content(text, &self.saver);
        let details = McpToolDetails { server: server.unwrap_or_default(), tool: self.name().into(), full_output_path };
        McpToolOutput { content, details, structured_content: payload, is_error: false }
    }

    async fn read(&self, args: &Value, cancel: &CancellationToken) -> anyhow::Result<McpToolOutput> {
        let name = string_argument(args, "server")?.ok_or_else(|| anyhow::anyhow!("server must be provided"))?;
        let uri = string_argument(args, "uri")?.ok_or_else(|| anyhow::anyhow!("uri must be provided"))?;
        let server = self.find_server(&name)?;
        let options =
            RequestOptions { cancel: Some(cancel.clone()), timeout: Some(server.timeout()), on_progress: None };
        let result = server.read_resource(&uri, options).await?;
        let name = server.name();
        // Several contents (for example a directory) are labeled with their URIs.
        let several = result.contents.len() > 1;
        let blocks: Vec<ContentBlock> = result
            .contents
            .iter()
            .flat_map(|contents| {
                let label = several.then(|| ContentBlock::text(format!("{}:", contents.uri)));
                label.into_iter().chain([ContentBlock::resource(contents.clone())])
            })
            .collect();
        let options = ConvertOptions { saver: self.saver.clone(), readable_resources: false };
        let mut converted = to_model_content(&name, &blocks, &options);
        if converted.is_empty() {
            converted.push(pi_ai::ContentBlock::text(format!("Resource {uri} is empty.")));
        }
        let (content, full_output_path) = limit_mcp_content(converted, &self.saver);
        let contents: Vec<Value> = result
            .contents
            .into_iter()
            .map(|contents| crate::types::ResourceContents { meta: None, ..contents })
            .map(|contents| json!(contents))
            .collect();
        let structured_content = json!({ "server": name, "uri": uri, "contents": contents });
        let details = McpToolDetails { server: name, tool: READ_MCP_RESOURCE_TOOL.into(), full_output_path };
        Ok(McpToolOutput { content, details, structured_content, is_error: false })
    }

    /// Run the tool; scripts get `structured_content`, the JSON payload.
    pub async fn call(&self, args: Value, cancel: CancellationToken) -> anyhow::Result<McpToolOutput> {
        if self.kind == Kind::Read {
            return self.read(&args, &cancel).await;
        }
        let payload = self.list(&args, &cancel).await?;
        Ok(self.json_result(string_argument(&args, "server")?, payload))
    }
}

#[async_trait]
impl AgentTool for McpResourceTool {
    fn name(&self) -> &str {
        match self.kind {
            Kind::Resources => LIST_MCP_RESOURCES_TOOL,
            Kind::Templates => LIST_MCP_RESOURCE_TEMPLATES_TOOL,
            Kind::Read => READ_MCP_RESOURCE_TOOL,
        }
    }

    fn description(&self) -> &str {
        match self.kind {
            Kind::Resources => schemas::LIST_DESCRIPTION,
            Kind::Templates => schemas::LIST_TEMPLATES_DESCRIPTION,
            Kind::Read => schemas::READ_DESCRIPTION,
        }
    }

    fn parameters(&self) -> Value {
        if self.kind == Kind::Read { schemas::read_parameters() } else { schemas::list_parameters() }
    }

    fn output_schema(&self) -> Option<Value> {
        Some(McpResourceTool::output_schema(self))
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn annotations(&self) -> Option<&ToolAnnotations> {
        Some(&READ_ONLY)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok(to_agent_result(self.call(args, cancel).await?))
    }
}
