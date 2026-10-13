//! Tool registry: exposure, active and callable sets, `default_active`, and `prepare_loadout`.

use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{
    AgentTool, AgentToolResult, RefreshOptions, SharedTool, ToolExposure, ToolLoadout, ToolLoadoutChanges,
    ToolNamespace, ToolRegistry, ToolUpdateCallback,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Spec {
    name: String,
    exposure: ToolExposure,
    default_active: bool,
    snippet: Option<String>,
    guidelines: Vec<String>,
    namespace: Option<ToolNamespace>,
    /// Lists the callable tools in its description and hides the named declarations.
    orchestrates: Option<Vec<String>>,
}

struct SpecTool(Spec);

fn spec(name: &str, exposure: ToolExposure) -> Spec {
    Spec { name: name.into(), exposure, default_active: true, ..Spec::default() }
}

fn tool(spec: Spec) -> SharedTool {
    Arc::new(SpecTool(spec))
}

#[async_trait]
impl AgentTool for SpecTool {
    fn name(&self) -> &str {
        &self.0.name
    }
    fn description(&self) -> &str {
        "original"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn exposure(&self) -> ToolExposure {
        self.0.exposure
    }
    fn namespace(&self) -> Option<&ToolNamespace> {
        self.0.namespace.as_ref()
    }
    fn default_active(&self) -> bool {
        self.0.default_active
    }
    fn prompt_snippet(&self) -> Option<&str> {
        self.0.snippet.as_deref()
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        self.0.guidelines.clone()
    }
    fn prepare_loadout(&self, loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        let hidden = self.0.orchestrates.clone()?;
        let callable: Vec<&str> = loadout.callable.iter().map(|tool| tool.name()).collect();
        let mut changes = ToolLoadoutChanges { hidden_declarations: hidden, ..Default::default() };
        changes.descriptions.insert(self.0.name.clone(), format!("calls {}", callable.join(",")));
        let declared: Vec<&str> = loadout.declared.iter().map(|tool| tool.name()).collect();
        changes.descriptions.insert("read".into(), format!("read, declared with {}", declared.join(",")));
        Some(changes)
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok(AgentToolResult::text(self.0.name.clone()))
    }
}

fn names(tools: &[SharedTool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name()).collect()
}

#[test]
fn registration_activates_direct_and_model_only_tools_by_default() {
    let registry = ToolRegistry::new([
        tool(spec("read", ToolExposure::Direct)),
        tool(spec("orchestrator", ToolExposure::ModelOnly)),
        tool(spec("mcp__docs__search", ToolExposure::Codemode)),
        tool(spec("mcp__late__x", ToolExposure::Deferred)),
        tool(spec("secret", ToolExposure::Hidden)),
        tool(Spec { default_active: false, ..spec("opt_in", ToolExposure::Direct) }),
    ]);
    assert_eq!(registry.active_names(), ["read", "orchestrator"]);
    assert_eq!(registry.default_active_names(), ["read", "orchestrator"]);
    // Active `direct` tools and every `codemode` or `deferred` tool are callable.
    assert_eq!(registry.callable_names(), ["read", "mcp__docs__search", "mcp__late__x"]);
    assert!(registry.is_declarable("opt_in") && !registry.is_activated_on_registration("opt_in"));
    assert_eq!(registry.exposure("unknown"), ToolExposure::Direct);
}

#[test]
fn set_active_tools_ignores_unknown_hidden_and_filtered_names() {
    let mut registry = ToolRegistry::new([
        tool(spec("read", ToolExposure::Direct)),
        tool(spec("bash", ToolExposure::Direct)),
        tool(spec("secret", ToolExposure::Hidden)),
        tool(spec("mcp__docs__search", ToolExposure::Codemode)),
    ]);
    let declared = registry.set_active_tools(&["bash", "secret", "nope", "mcp__docs__search", "bash"]);
    assert_eq!(names(&declared), ["bash", "mcp__docs__search"]);
    // Deactivating a direct tool makes it uncallable; codemode tools stay callable.
    assert_eq!(registry.callable_names(), ["bash", "mcp__docs__search"]);
    registry.set_activation_filter(Some(Arc::new(|name, _| !name.starts_with("mcp__"))));
    assert_eq!(registry.active_names(), ["bash"]);
}

#[test]
fn refresh_keeps_the_active_set_and_activates_new_tools() {
    let mut registry =
        ToolRegistry::new([tool(spec("read", ToolExposure::Direct)), tool(spec("bash", ToolExposure::Direct))]);
    registry.set_active_tools(&["read"]);
    registry.register(tool(spec("grep", ToolExposure::Direct)));
    registry.register(tool(spec("mcp__x", ToolExposure::Deferred)));
    assert_eq!(registry.active_names(), ["read", "grep"]);
    // A tool whose exposure changes to a declarable one is activated like a new tool.
    registry.register(tool(spec("mcp__x", ToolExposure::Direct)));
    assert_eq!(registry.active_names(), ["read", "grep", "mcp__x"]);
    registry.unregister("grep");
    assert_eq!(registry.active_names(), ["read", "mcp__x"]);
    let tools = registry.tools().to_vec();
    registry.refresh(
        tools,
        RefreshOptions { active_tool_names: Some(vec!["bash".into()]), extra_active: vec!["read".into()] },
    );
    assert_eq!(registry.active_names(), ["bash", "read"]);
}

#[test]
fn prepare_loadout_replaces_descriptions_and_hides_declarations() {
    let orchestrator = Spec { orchestrates: Some(vec!["read".into()]), ..spec("codemode", ToolExposure::ModelOnly) };
    let mut registry = ToolRegistry::new([
        tool(spec("read", ToolExposure::Direct)),
        tool(orchestrator),
        tool(spec("mcp__docs__search", ToolExposure::Codemode)),
    ]);
    let declared = registry.set_active_tools(&["read", "codemode"]);
    let descriptions: Vec<&str> = declared.iter().map(|tool| tool.description()).collect();
    assert_eq!(descriptions, ["read, declared with read,codemode", "calls read,mcp__docs__search"]);
    assert_eq!(registry.hidden_declarations(), ["read"]);
    // The wrapper still runs the tool and keeps its metadata.
    assert_eq!(declared[1].exposure(), ToolExposure::ModelOnly);
    assert_eq!(declared[0].declaration().description, "read, declared with read,codemode");
    // Without the hook's tool, descriptions are the originals and nothing is hidden.
    let declared = registry.set_active_tools(&["read"]);
    assert_eq!(declared[0].description(), "original");
    assert!(registry.hidden_declarations().is_empty());
}

#[test]
fn prompt_snippets_guidelines_and_infos() {
    let documented = Spec {
        snippet: Some("Read\n  file   contents".into()),
        guidelines: vec![" Use read. ".into(), "Use read.".into(), "  ".into()],
        namespace: Some(ToolNamespace::new("fs")),
        ..spec("read", ToolExposure::Direct)
    };
    let registry = ToolRegistry::new([tool(documented), tool(spec("bash", ToolExposure::Direct))]);
    assert_eq!(registry.prompt_snippets().get("read").map(String::as_str), Some("Read file contents"));
    assert!(!registry.prompt_snippets().contains_key("bash"));
    assert_eq!(registry.prompt_guidelines()["read"], ["Use read."]);
    let infos = registry.tool_infos();
    let value = serde_json::to_value(&infos[0]).unwrap();
    assert_eq!(value["exposure"], "direct");
    assert_eq!(value["namespace"]["name"], "fs");
    assert!(serde_json::to_value(&infos[1]).unwrap().get("promptGuidelines").is_none());
    let loadout = registry.loadout(registry.active_tools().to_vec());
    assert_eq!(loadout.prompt_guidelines("read"), ["Use read."]);
    assert_eq!(loadout.namespace("read").map(|namespace| namespace.name), Some("fs".to_string()));
}
