//! Generic workspace tools over a [`WorkspaceAdapter`].

use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::JsonObject;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::host::{EmbeddedTool, ToolContext, ToolDeclaration, WorkspaceAdapter};
use crate::types::{Reconciliation, ToolEffect, ToolInvocation, ToolResult};

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

fn declaration(
    name: &str,
    description: &str,
    parameters: Value,
    capability: &str,
    effect: ToolEffect,
) -> ToolDeclaration {
    ToolDeclaration {
        name: name.into(),
        description: description.into(),
        parameters,
        capabilities: vec![capability.into()],
        effect,
        approval: effect == ToolEffect::Mutation,
    }
}

/// Upstream reads arguments with `String(...)`; validated schemas make these strings.
fn text(args: &JsonObject, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => "undefined".into(),
    }
}

/// The session-qualified operation identifier passed to edits and reconciliation.
fn operation_id(session_id: &str, invocation_id: &str) -> String {
    json!([session_id, invocation_id]).to_string()
}

#[derive(Clone, Copy)]
enum Kind {
    Read,
    Search,
    Edit,
    Diagnostics,
    Navigate,
}

struct WorkspaceTool {
    workspace: Arc<dyn WorkspaceAdapter>,
    declaration: ToolDeclaration,
    kind: Kind,
}

#[async_trait]
impl EmbeddedTool for WorkspaceTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    async fn execute(&self, args: JsonObject, context: ToolContext) -> anyhow::Result<ToolResult> {
        let workspace = &self.workspace;
        let cancel = context.cancel.clone();
        let value = match self.kind {
            Kind::Read => serde_json::to_value(workspace.read(&text(&args, "path"), cancel).await?)?,
            Kind::Search => serde_json::to_value(workspace.search(&text(&args, "query"), cancel).await?)?,
            Kind::Edit => {
                let operation = operation_id(&context.session_id, &context.invocation_id);
                let (path, revision) = (text(&args, "path"), text(&args, "expectedRevision"));
                let document = workspace.edit(&path, &revision, &text(&args, "text"), &operation, cancel).await?;
                serde_json::to_value(document)?
            }
            Kind::Diagnostics => workspace.diagnostics(&text(&args, "path"), cancel).await?,
            Kind::Navigate => {
                let line = args.get("line").and_then(Value::as_u64).unwrap_or_default();
                workspace.navigate(&text(&args, "path"), line, cancel).await?
            }
        };
        Ok(ToolResult::json(value))
    }

    async fn reconcile(&self, invocation: ToolInvocation, cancel: CancellationToken) -> anyhow::Result<Reconciliation> {
        match self.kind {
            Kind::Edit => {
                let id = operation_id(&invocation.session_id, &invocation.id);
                self.workspace.reconcile_edit(ToolInvocation { id, ..invocation }, cancel).await
            }
            _ => Ok(Reconciliation::Unknown),
        }
    }
}

/// `read_file`, `search_files`, and reviewed `edit_file`, plus `diagnostics` and `navigate`
/// when the workspace supports them. Each requires its `workspace.*` capability.
pub fn create_workspace_tools(workspace: Arc<dyn WorkspaceAdapter>) -> Vec<Arc<dyn EmbeddedTool>> {
    let path = || schema(json!({"path": {"type": "string"}}), &["path"]);
    let mut specs = vec![
        (
            Kind::Read,
            declaration(
                "read_file",
                "Read a workspace document and its revision, including unsaved host contents.",
                path(),
                "workspace.read",
                ToolEffect::Read,
            ),
        ),
        (
            Kind::Search,
            declaration(
                "search_files",
                "Search text in the host workspace.",
                schema(json!({"query": {"type": "string"}}), &["query"]),
                "workspace.search",
                ToolEffect::Read,
            ),
        ),
        (
            Kind::Edit,
            declaration(
                "edit_file",
                "Replace a document after host review and an atomic revision check. Read its current revision first.",
                schema(
                    json!({"path": {"type": "string"}, "expectedRevision": {"type": "string"}, "text": {"type": "string"}}),
                    &["path", "expectedRevision", "text"],
                ),
                "workspace.edit",
                ToolEffect::Mutation,
            ),
        ),
    ];
    if workspace.supports_diagnostics() {
        let description = "Read host diagnostics for a document.";
        let tool = declaration("diagnostics", description, path(), "workspace.diagnostics", ToolEffect::Read);
        specs.push((Kind::Diagnostics, tool));
    }
    if workspace.supports_navigation() {
        let parameters =
            schema(json!({"path": {"type": "string"}, "line": {"type": "integer", "minimum": 1}}), &["path", "line"]);
        let description = "Resolve a document location using the host's navigation index.";
        specs.push((
            Kind::Navigate,
            declaration("navigate", description, parameters, "workspace.navigate", ToolEffect::Read),
        ));
    }
    specs
        .into_iter()
        .map(|(kind, declaration)| {
            Arc::new(WorkspaceTool { workspace: workspace.clone(), declaration, kind }) as Arc<dyn EmbeddedTool>
        })
        .collect()
}
