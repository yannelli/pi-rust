//! Typed requests: tools, resources, and paginated lists.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use super::{McpClient, RequestOptions, validate};
use crate::content::CallToolResult;
use crate::error::McpError;
use crate::types::{
    ListResourceTemplatesResult, ListResourcesResult, ReadResourceResult, Resource, ResourceTemplate, Tool,
};

const MAX_LIST_PAGES: usize = 1_000;

impl McpClient {
    pub async fn ping(&self, options: RequestOptions) -> Result<(), McpError> {
        self.request("ping", None, options).await.map(|_| ())
    }

    pub async fn list_tools(&self, options: RequestOptions) -> Result<Vec<Tool>, McpError> {
        validate::tools(self.list_all("tools/list", "tools", validate::is_tool, &options).await?)
    }

    /// Every resource, following `nextCursor` through all pages.
    pub async fn list_resources(&self, options: RequestOptions) -> Result<Vec<Resource>, McpError> {
        validate::resources(self.list_all("resources/list", "resources", validate::is_resource, &options).await?)
    }

    /// One page of resources, starting at `cursor`.
    pub async fn list_resources_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourcesResult, McpError> {
        let page = self.list_page("resources/list", "resources", validate::is_resource, cursor, &options).await?;
        Ok(ListResourcesResult { resources: validate::resources(page.items)?, next_cursor: page.next_cursor })
    }

    /// Every resource template, following `nextCursor` through all pages.
    pub async fn list_resource_templates(&self, options: RequestOptions) -> Result<Vec<ResourceTemplate>, McpError> {
        let items = self
            .list_all("resources/templates/list", "resourceTemplates", validate::is_resource_template, &options)
            .await?;
        validate::resource_templates(items)
    }

    /// One page of resource templates, starting at `cursor`.
    pub async fn list_resource_templates_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let page = self
            .list_page(
                "resources/templates/list",
                "resourceTemplates",
                validate::is_resource_template,
                cursor,
                &options,
            )
            .await?;
        let resource_templates = validate::resource_templates(page.items)?;
        Ok(ListResourceTemplatesResult { resource_templates, next_cursor: page.next_cursor })
    }

    pub async fn read_resource(&self, uri: &str, options: RequestOptions) -> Result<ReadResourceResult, McpError> {
        validate::read_resource_result(self.request("resources/read", Some(json!({ "uri": uri })), options).await?)
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Option<Value>,
        options: RequestOptions,
    ) -> Result<CallToolResult, McpError> {
        let mut params = Map::new();
        params.insert("name".into(), json!(name));
        if let Some(arguments) = arguments {
            params.insert("arguments".into(), arguments);
        }
        validate::call_tool_result(self.request("tools/call", Some(Value::Object(params)), options).await?)
    }

    async fn list_page(
        &self,
        method: &str,
        key: &str,
        is_item: fn(&Value) -> bool,
        cursor: Option<&str>,
        options: &RequestOptions,
    ) -> Result<validate::Page, McpError> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        validate::list_page(method, key, self.request(method, params, options.clone()).await?, is_item)
    }

    /// Every item of a paginated list method.
    async fn list_all(
        &self,
        method: &str,
        key: &str,
        is_item: fn(&Value) -> bool,
        options: &RequestOptions,
    ) -> Result<Vec<Value>, McpError> {
        let mut items = Vec::new();
        let mut cursors = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let page = self.list_page(method, key, is_item, cursor.as_deref(), options).await?;
            items.extend(page.items);
            let Some(next) = page.next_cursor else { return Ok(items) };
            if !cursors.insert(next.clone()) {
                return Err(McpError::other(format!("MCP {method} returned duplicate cursor: {next}")));
            }
            cursor = Some(next);
        }
        Err(McpError::other(format!("MCP {method} exceeded {MAX_LIST_PAGES} pages")))
    }
}
