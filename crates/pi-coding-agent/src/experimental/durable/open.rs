//! `openDurable` of upstream `durable/runtime.ts` and `vacation/runtime.ts`, and their `main.ts` arguments.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pi_chord::BACKGROUND_CONTEXT;
use pi_durable::harness::types::{AgentChange, Change};
use pi_durable::harness::{CreateOptions, Harness, HarnessOptions, RegistryReader};
use pi_durable::storage::open_sqlite_storage;
use pi_durable::{DurableError, DurableResult};

use super::controller::DurableController;
use super::harness_setup::{
    ExecutionEnvs, configure_harness_http, create_coding_registry, create_harness_settings, find_initial_agent_model,
    runtime_models,
};
use super::runtime::{NoticeLevel, SessionSummary, ViewStore, open_view};
use super::sessions::{SessionLocation, select_session};
use super::subagent::subagent;
use crate::agent_session::{ModelRuntime, ModelRuntimeOptions};
use crate::experimental::vacation::{VACATION_EXTENSION, create_vacation_registry};
use crate::settings::{SettingsManager, SettingsOptions};

/// The app on the durable runtime: the coding agent of `durable/`, or the vacation planner of `vacation/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableApp {
    Coding,
    Vacation,
}

impl DurableApp {
    fn kind(self) -> &'static str {
        match self {
            Self::Coding => "durable",
            Self::Vacation => "vacation",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OpenDurableOptions {
    pub cwd: Option<PathBuf>,
    pub continue_session: bool,
    /// Defaults to pi's agent directory.
    pub agent_dir: Option<PathBuf>,
}

/// Upstream `parseArgs` of `main.ts`.
pub fn parse_durable_args(args: &[String]) -> DurableResult<OpenDurableOptions> {
    let mut options = OpenDurableOptions::default();
    for arg in args {
        match arg.as_str() {
            "--continue" | "-c" => options.continue_session = true,
            _ => return Err(DurableError::Error(format!("Unknown argument: {arg}"))),
        }
    }
    Ok(options)
}

pub struct OpenDurableResult {
    pub view: Arc<ViewStore>,
    pub controller: DurableController,
    /// pi's settings, for the TUI's theme and terminal capabilities.
    pub settings: Arc<SettingsManager>,
    harness: Harness,
    envs: Option<ExecutionEnvs>,
    location: SessionLocation,
}

impl OpenDurableResult {
    /// Close writes no outcome: a running turn resumes with `--continue`.
    pub async fn close(mut self) -> DurableResult<()> {
        self.controller.state().detach();
        let closed = self.harness.close(&BACKGROUND_CONTEXT).await;
        if let Some(envs) = &self.envs {
            envs.cleanup(&BACKGROUND_CONTEXT).await;
        }
        let released = self.location.release().await;
        closed.and(released)
    }
}

pub async fn open_durable(app: DurableApp, options: OpenDurableOptions) -> DurableResult<OpenDurableResult> {
    let agent_dir = options.agent_dir.clone().unwrap_or_else(crate::config::agent_dir);
    let cwd = match options.cwd.clone() {
        Some(cwd) => cwd,
        None => std::env::current_dir().map_err(|error| DurableError::Error(error.to_string()))?,
    };
    let mut location = select_session(&agent_dir, app.kind(), &cwd, options.continue_session).await?;
    let envs = (app == DurableApp::Coding).then(|| ExecutionEnvs::new(location.cwd.clone()));
    let mut harness: Option<Harness> = None;
    let opened = open(app, &agent_dir, &location, envs.clone(), &mut harness).await;
    match opened {
        Ok((view, controller, settings)) => {
            let harness = harness.unwrap_or_else(|| unreachable!("opened Harness"));
            Ok(OpenDurableResult { view, controller, settings, harness, envs, location })
        }
        Err(error) => {
            if let Some(harness) = harness {
                let _ = harness.close(&BACKGROUND_CONTEXT).await;
            }
            let _ = location.release().await;
            Err(error)
        }
    }
}

type Opened = (Arc<ViewStore>, DurableController, Arc<SettingsManager>);

/// The view once it exists, and the reports that came before it.
type Reports = (Option<Arc<ViewStore>>, Vec<String>);

async fn open(
    app: DurableApp,
    agent_dir: &std::path::Path,
    location: &SessionLocation,
    envs: Option<ExecutionEnvs>,
    slot: &mut Option<Harness>,
) -> DurableResult<Opened> {
    let model_runtime = Arc::new(ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(agent_dir)).await);
    let cwd_path = PathBuf::from(&location.cwd);
    let settings = Arc::new(SettingsManager::create(&cwd_path, agent_dir, SettingsOptions::default()));
    configure_harness_http(&settings);
    let registry = match app {
        DurableApp::Coding => {
            let registry = create_coding_registry(settings.clone(), agent_dir, &location.cwd)?;
            registry.install(subagent())?;
            registry
        }
        DurableApp::Vacation => create_vacation_registry()?,
    };

    // Reports that arrive before the view exists are kept and shown once it does.
    let store: Arc<Mutex<Reports>> = Arc::default();
    let reports = store.clone();
    let registry: Arc<dyn RegistryReader> = Arc::new(registry);
    let mut harness_options =
        HarnessOptions::new(runtime_models(&model_runtime), registry).with_on_report(move |error| {
            let mut reports = reports.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match &reports.0 {
                Some(view) => view.notice(NoticeLevel::Warning, error.to_string()),
                None => reports.1.push(error.to_string()),
            }
        });
    harness_options.settings = Some(create_harness_settings(settings.clone()));
    if let Some(envs) = &envs {
        harness_options = harness_options.with_env(envs.builder());
    }
    let storage = Arc::new(open_sqlite_storage(&location.database.display().to_string()).await?);
    let harness = slot.insert(Harness::open(storage, harness_options, &BACKGROUND_CONTEXT).await?).clone();

    let initial = match location.created {
        true => Some(find_initial_agent_model(&settings, &model_runtime, None)?),
        false => None,
    };
    let mut agent = AgentChange::default();
    match app {
        DurableApp::Coding => agent.cwd = Change::Set(location.cwd.clone()),
        // Only the vacation planner; the research subagent selects search itself.
        DurableApp::Vacation => {
            let extensions = pi_durable::harness::types::ExtensionSelection::List(vec![VACATION_EXTENSION.into()]);
            agent.extensions = Change::Set(extensions);
        }
    }
    if let Some(model) = initial.as_ref().and_then(|initial| initial.model.clone()) {
        agent.model = Change::Set(model);
    }
    if let Some(level) = initial.as_ref().and_then(|initial| initial.thinking_level) {
        agent.thinking_level = Change::Set(level);
    }
    let root = harness.root(&BACKGROUND_CONTEXT, CreateOptions::default().with_agent(agent)).await?;
    let session =
        SessionSummary { id: location.id.clone(), directory: location.directory.clone(), cwd: location.cwd.clone() };
    let (view, controller) = open_view(harness, root, model_runtime, session).await?;
    {
        let mut reports = store.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for report in reports.1.drain(..) {
            view.notice(NoticeLevel::Warning, report);
        }
        reports.0 = Some(view.clone());
    }
    if let Some(message) = initial.and_then(|initial| initial.fallback_message) {
        view.notice(NoticeLevel::Info, message);
    }
    Ok((view, controller, settings))
}
