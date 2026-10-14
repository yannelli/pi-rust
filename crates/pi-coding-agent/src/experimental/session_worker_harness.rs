//! Upstream `createCodingAgentHarness` of `session-worker.ts`: the coding agent Harness a Session worker opens,
//! with pi's model runtime, settings, coding registry, and one execution environment per directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi_chord::{ChordError, ChordResult, TODO_CONTEXT};
use pi_durable::harness::types::{AgentChange, Change};
use pi_durable::harness::{CreateOptions, Harness, HarnessOptions, RegistryReader};
use pi_durable::storage::open_sqlite_storage;
use pi_durable::{DurableError, ROOT_CONVERSATION_ID};

use super::durable::harness_setup::{
    ExecutionEnvs, configure_harness_http, create_coding_registry, create_harness_settings, find_initial_agent_model,
    runtime_models,
};
use super::plugins::bundled::create_session_plugin_facet_loader;
use super::services::models::ModelDefaults;
use super::services::worker::SessionWorkerRuntime;
use super::session_worker::SessionWorkerOptions;
use super::session_worker_run::CreateSessionWorkerHarness;
use crate::agent_session::{ModelRuntime, ModelRuntimeOptions};
use crate::settings::{SettingsManager, SettingsOptions};

fn chord(error: DurableError) -> ChordError {
    ChordError::other(error)
}

/// The `ModelDefaults` of the Models service over pi's settings: a selection becomes the default model.
pub struct SettingsModelDefaults(pub Arc<SettingsManager>);

#[async_trait]
impl ModelDefaults for SettingsModelDefaults {
    async fn set_default_model_and_provider(&self, provider: &str, model_id: &str) -> ChordResult<()> {
        self.0.set_default_model_and_provider(provider, model_id);
        Ok(())
    }
}

/// Upstream `createCodingAgentHarness` with pi's agent directory as `agent_dir`.
pub fn create_coding_agent_harness(agent_dir: PathBuf) -> CreateSessionWorkerHarness {
    Arc::new(move |database_path, options| {
        let agent_dir = agent_dir.clone();
        Box::pin(async move { open_coding_agent_harness(&agent_dir, database_path, options).await })
    })
}

async fn open_coding_agent_harness(
    agent_dir: &Path,
    database_path: PathBuf,
    options: SessionWorkerOptions,
) -> ChordResult<SessionWorkerRuntime> {
    let cwd = options.metadata.cwd.clone();
    let model_runtime = Arc::new(ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(agent_dir)).await);
    let settings = Arc::new(SettingsManager::create(Path::new(&cwd), agent_dir, SettingsOptions::default()));
    configure_harness_http(&settings);
    let envs = ExecutionEnvs::new(cwd.clone());
    let mut opened: Option<Harness> = None;
    let result = async {
        let registry: Arc<dyn RegistryReader> =
            Arc::new(create_coding_registry(settings.clone(), agent_dir, &cwd).map_err(chord)?);
        let mut harness_options = HarnessOptions::new(runtime_models(&model_runtime), registry)
            .with_env(envs.builder())
            .with_on_report(|error| eprintln!("{error}"));
        harness_options.settings = Some(create_harness_settings(settings.clone()));
        let storage = Arc::new(open_sqlite_storage(&database_path.display().to_string()).await.map_err(chord)?);
        let harness = opened.insert(Harness::open(storage, harness_options, &TODO_CONTEXT).await.map_err(chord)?);
        // The start model applies only when the root conversation is created; later starts keep its durable model.
        let created = harness.conversation(ROOT_CONVERSATION_ID, &TODO_CONTEXT).await.map_err(chord)?.is_none();
        let mut agent = AgentChange { cwd: Change::Set(cwd.clone()), ..AgentChange::default() };
        if created {
            let cli = options.model.as_deref().map(|model| (options.provider.as_deref(), model));
            let start = find_initial_agent_model(&settings, &model_runtime, cli).map_err(chord)?;
            if let Some(model) = start.model {
                agent.model = Change::Set(model);
            }
            if let Some(level) = start.thinking_level {
                agent.thinking_level = Change::Set(level);
            }
        }
        let conversation =
            harness.root(&TODO_CONTEXT, CreateOptions::default().with_agent(agent)).await.map_err(chord)?;
        Ok::<_, ChordError>((harness.clone(), conversation))
    }
    .await;
    match result {
        Ok((harness, conversation)) => {
            let cleanup_envs = envs.clone();
            Ok(SessionWorkerRuntime {
                harness,
                conversation,
                model_catalog: Some(model_runtime),
                model_defaults: Some(Arc::new(SettingsModelDefaults(settings))),
                facet_loader: create_session_plugin_facet_loader(&options.plugin_manifest_paths),
                cleanup: Some(Box::new(move |context| {
                    Box::pin(async move {
                        cleanup_envs.cleanup(&context).await;
                        Ok(())
                    })
                })),
            })
        }
        Err(error) => {
            if let Some(harness) = opened
                && let Err(cleanup) = harness.close(&TODO_CONTEXT).await
            {
                envs.cleanup(&TODO_CONTEXT).await;
                let message = "Session worker Harness startup and cleanup failed";
                return Err(ChordError::with_cleanup(error, vec![chord(cleanup)], message));
            }
            envs.cleanup(&TODO_CONTEXT).await;
            Err(error)
        }
    }
}
