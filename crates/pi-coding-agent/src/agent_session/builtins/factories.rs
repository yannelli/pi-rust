//! The SDK factories `createCodemodeExtension`, `createToolSearchExtension`, and
//! `createMcpExtension` (upstream `extensions/*/index.ts`) over the native built-ins: the tools
//! they register run the native `codemode` and `tool_search` of a session, and the MCP factory
//! starts the session's native MCP support.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use pi_agent::SharedTool;
use pi_codemode::CodemodeMode;
use serde_json::{Map, Value, json};

use super::codemode::CodemodeOverrides;
use super::codemode_host::SessionCodemodeHost;
use super::{SessionCodemodeTool, ToolSearchTool, codemode_docs_path};
use crate::agent_session::model_runtime::ModelRuntime;
use crate::agent_session::{AgentSession, lock};
use crate::settings::SettingsManager;

/// Upstream reads `mode` as `only` or anything else, and takes any `inlineBudget` number.
fn codemode_overrides(options: &Value) -> CodemodeOverrides {
    let mode = options["mode"].as_str().map(|mode| if mode == "only" { CodemodeMode::Only } else { CodemodeMode::On });
    let budget = options["inlineBudget"].as_f64().filter(|budget| budget.is_finite() && *budget >= 0.0);
    CodemodeOverrides { mode, inline_budget: budget.map(|budget| budget as usize), models: options["models"].as_bool() }
}

fn build(
    session: Weak<AgentSession>,
    runtime: Arc<ModelRuntime>,
    settings: Arc<SettingsManager>,
    agent_dir: &Path,
    name: &str,
    options: &Value,
) -> Option<SharedTool> {
    match name {
        "codemode" => {
            let host = Arc::new(SessionCodemodeHost::new(session, runtime));
            let docs = codemode_docs_path(agent_dir);
            Some(Arc::new(SessionCodemodeTool::with_overrides(host, settings, &docs, codemode_overrides(options))))
        }
        "tool_search" => Some(Arc::new(ToolSearchTool::new(session))),
        _ => None,
    }
}

/// The definition fields of factory tool `name` before a session exists, as the factory registers it.
pub(crate) fn factory_definition(
    runtime: Arc<ModelRuntime>,
    agent_dir: &Path,
    name: &str,
    options: &Value,
) -> Option<Value> {
    let settings = Arc::new(SettingsManager::in_memory(json!({})));
    let tool = build(Weak::new(), runtime, settings, agent_dir, name, options)?;
    let mut definition = Map::new();
    definition.insert("name".into(), json!(tool.name()));
    definition.insert("label".into(), json!(tool.label()));
    definition.insert("description".into(), json!(tool.description()));
    definition.insert("parameters".into(), tool.parameters());
    definition.insert("promptGuidelines".into(), json!(tool.prompt_guidelines()));
    definition.insert("exposure".into(), json!(tool.exposure()));
    if let Some(snippet) = tool.prompt_snippet() {
        definition.insert("promptSnippet".into(), json!(snippet));
    }
    if let Some(sampling) = tool.constrained_sampling() {
        definition.insert("constrainedSampling".into(), sampling);
    }
    Some(Value::Object(definition))
}

impl AgentSession {
    /// The native tool behind factory tool `name` (`codemode`, `tool_search`) for this session.
    /// Unbound, it reads only the settings, so loadout hooks can run while the tools are locked.
    pub(crate) fn factory_tool(&self, name: &str, options: &Value, bound: bool) -> Option<SharedTool> {
        let session = if bound { self.weak.clone() } else { Weak::new() };
        let runtime = self.model_runtime().clone();
        build(session, runtime, self.settings().clone(), &self.services.agent_dir, name, options)
    }

    /// `createMcpExtension` on `session_start`: run native MCP for this session, also when
    /// `builtin:mcp` is off, with the factory's `logPath` and `startupWaitMs`.
    pub(crate) fn start_factory_mcp(&self, options: &Value) {
        {
            let mut builtins = lock(&self.builtins);
            builtins.factory_mcp = true;
            if builtins.mcp.is_some() {
                return;
            }
            if let Some(path) = options["logPath"].as_str() {
                builtins.mcp_options.log_path = Some(PathBuf::from(path));
            }
            let wait = options["startupWaitMs"].as_f64().filter(|wait| wait.is_finite() && *wait >= 0.0);
            if let Some(wait) = wait {
                builtins.mcp_options.startup_wait = Some(Duration::from_millis(wait as u64));
            }
        }
        self.start_mcp();
    }
}
