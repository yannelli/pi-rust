use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi_js::JsError;
use serde_json::{Value, json};

use super::ops::error;
use super::ops_sdk::ChildSession;
use super::ops_sdk_resources::Hosted;
use super::runner::ExtensionRunner;
use crate::agent_session::services::resource_loader_options;
use crate::agent_session::{
    AgentSessionServices, CreateAgentSessionOptions, ModelRuntime, NoTools, ResourceOptions, create_agent_session, lock,
};
use crate::resources::ResourceLoader;
use crate::settings::SettingsManager;

struct SessionReservation<'a> {
    runner: &'a ExtensionRunner,
    manager_id: u64,
}

impl Drop for SessionReservation<'_> {
    fn drop(&mut self) {
        lock(&self.runner.sdk.storage).cancel_create(self.manager_id);
    }
}

fn strings(value: &Value) -> Result<Vec<String>, JsError> {
    if value.is_null() { Ok(Vec::new()) } else { serde_json::from_value(value.clone()).map_err(error) }
}

pub(crate) async fn create(runner: &Arc<ExtensionRunner>, options: Value) -> Result<Value, JsError> {
    let manager_id = options["sessionManagerId"].as_u64().ok_or_else(|| error("Missing SessionManager"))?;
    let settings_id = options["settingsManagerId"].as_u64().ok_or_else(|| error("Missing SettingsManager"))?;
    let model_runtime = runner.sdk_model_runtime(options["modelRuntimeId"].as_u64().unwrap_or(0))?;
    let model = options.get("model").cloned().map(serde_json::from_value).transpose().map_err(error)?;
    let thinking_level =
        options.get("thinkingLevel").cloned().map(serde_json::from_value).transpose().map_err(error)?;
    let tools = options.get("tools").map(strings).transpose()?;
    let exclude_tools = strings(&options["excludeTools"])?;
    let no_tools = match options["noTools"].as_str() {
        Some("all") => Some(NoTools::All),
        Some("builtin") => Some(NoTools::Builtin),
        None => None,
        Some(other) => return Err(error(format!("Invalid noTools option: {other}"))),
    };
    let (manager, settings) = {
        let mut storage = lock(&runner.sdk.storage);
        let settings = storage.settings(settings_id).ok_or_else(|| error("Unknown SettingsManager"))?;
        let manager = storage.take_session(manager_id)?;
        (manager, settings)
    };
    let _reservation = SessionReservation { runner, manager_id };
    let cwd = PathBuf::from(options["cwd"].as_str().unwrap_or_else(|| runner.cwd().to_str().unwrap_or(".")));
    let agent_dir = options["agentDir"].as_str().map(PathBuf::from).unwrap_or_else(|| runner.agent_dir().to_owned());
    let resources = &options["resources"];
    let (resource_loader, hosted) = match resources["resourceLoaderId"].as_u64() {
        Some(id) => super::ops_sdk_resources::take(runner, id)?,
        None => (fresh_loader(&cwd, &agent_dir, &settings, resources)?, None),
    };
    if let Some((hosted, _)) = &hosted {
        bind_models(runner, hosted, &model_runtime).await?;
    }
    let services =
        AgentSessionServices { cwd, agent_dir, settings, model_runtime, resource_loader, diagnostics: Vec::new() };
    let mut create = CreateAgentSessionOptions::new(services, manager);
    create.extensions = hosted.as_ref().map(|(hosted, _)| hosted.0.clone());
    create.model = model;
    create.thinking_level = thinking_level;
    create.tools = tools;
    create.exclude_tools = exclude_tools;
    create.no_tools = no_tools;
    let created = create_agent_session(create).await.map_err(error)?;
    let id = runner.next_id();
    let (extensions, target) = hosted.map_or((None, 0), |(hosted, target)| (Some(hosted), target));
    if target != 0 {
        lock(&runner.targets).insert(target, Arc::downgrade(&created.session));
    }
    lock(&runner.sdk.storage).attach_child(manager_id, &created.session);
    let child = ChildSession { session: created.session, manager_id, extensions };
    lock(&runner.sdk.children).insert(id, child);
    Ok(json!({"id": id, "target": target, "modelFallbackMessage": created.model_fallback_message}))
}

/// Bind the providers and virtual models of a loader's extensions to the session's `runtime`: file
/// extensions through their child host, factories through this host (`resources.models`).
async fn bind_models(runner: &ExtensionRunner, hosted: &Hosted, runtime: &Arc<ModelRuntime>) -> Result<(), JsError> {
    if !hosted.0.select_model_runtime(runtime.clone()) {
        return Err(error("The extensions of this reload already bound a model runtime"));
    }
    hosted.0.call_json("bindModels", Vec::new()).await?;
    runner.call_json("sdk", vec![json!("models"), json!(hosted.group())]).await?;
    runtime.wait_for_offline_refreshes().await;
    Ok(())
}

/// The loader of a child session created without an SDK `DefaultResourceLoader`.
fn fresh_loader(
    cwd: &Path,
    agent_dir: &Path,
    settings: &SettingsManager,
    resources: &Value,
) -> Result<Arc<Mutex<ResourceLoader>>, JsError> {
    let resource_options = ResourceOptions {
        no_extensions: true,
        no_skills: resources["noSkills"].as_bool().unwrap_or(false),
        no_prompt_templates: resources["noPromptTemplates"].as_bool().unwrap_or(false),
        no_themes: resources["noThemes"].as_bool().unwrap_or(false),
        no_context_files: resources["noContextFiles"].as_bool().unwrap_or(false),
        system_prompt: resources["systemPrompt"].as_str().map(str::to_owned),
        append_system_prompt: resources.get("appendSystemPrompt").map(strings).transpose()?,
        skill_paths: strings(&resources["additionalSkillPaths"])?,
        prompt_template_paths: strings(&resources["additionalPromptTemplatePaths"])?,
        disabled_builtin_extensions: crate::agent_session::BUILTIN_EXTENSIONS
            .iter()
            .map(|name| name.to_string())
            .collect(),
        ..Default::default()
    };
    let mut loader = ResourceLoader::new(resource_loader_options(cwd, agent_dir, settings, &resource_options));
    loader.reload();
    Ok(Arc::new(Mutex::new(loader)))
}
