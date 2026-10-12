//! A coding session: the agent, its tools and prompt, session persistence,
//! model selection, and automatic compaction.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use pi_agent::{Agent, AgentEvent, AgentHooks};
use pi_ai::{ContentBlock, Message, Model, ModelRegistry, ThinkingLevel, is_context_overflow};
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    self, CompactionResult, CompactionSettings, context_tokens, prepare_compaction, should_compact,
};
use crate::config::{ContextFile, Settings, load_context_files, models_path};
use crate::session::{SessionManager, session_dir};
use crate::system_prompt::{SystemPromptOptions, build_system_prompt};
use crate::tools;

/// Providers tried, in order, when no model is configured.
const DEFAULT_MODELS: &[(&str, &str)] = &[
    ("anthropic", "claude-opus-4-8"),
    ("openai", "gpt-5.5"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("deepseek", "deepseek-v4-pro"),
    ("xai", "grok-4.7"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "gpt-oss-120b"),
    ("zai", "glm-5.3"),
    ("mistral", "devstral-medium-latest"),
    ("moonshotai", "kimi-k2.6"),
];

#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    /// `None` uses the default tools.
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_context_files: bool,
    /// Session file to open.
    pub session_file: Option<PathBuf>,
    pub continue_recent: bool,
    pub persist: bool,
    pub session_dir: Option<PathBuf>,
}

pub struct CodingSession {
    pub agent: Agent,
    session: Arc<Mutex<SessionManager>>,
    registry: ModelRegistry,
    api_key: Option<String>,
    pub compaction: CompactionSettings,
    cwd: PathBuf,
    sessions_dir: Option<PathBuf>,
    pub context_files: Vec<ContextFile>,
    pub tool_names: Vec<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resolve the model: explicit flags, then the session's last model, then settings, then defaults with a key.
pub fn resolve_model(
    registry: &ModelRegistry,
    provider: Option<&str>,
    pattern: Option<&str>,
    session_model: Option<&(String, String)>,
    settings: &Settings,
) -> anyhow::Result<Model> {
    if let Some(pattern) = pattern {
        return registry
            .resolve(pattern, provider)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No model matches \"{pattern}\""));
    }
    if let Some(provider) = provider {
        let preferred =
            DEFAULT_MODELS.iter().find(|(name, _)| *name == provider).and_then(|(_, id)| registry.find(provider, id));
        return preferred
            .or_else(|| registry.models().into_iter().find(|model| model.provider == provider))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown provider: {provider}"));
    }
    if let Some(model) = session_model.and_then(|(provider, id)| registry.find(provider, id)) {
        return Ok(model.clone());
    }
    if let (Some(provider), Some(id)) = (&settings.default_provider, &settings.default_model)
        && let Some(model) = registry.find(provider, id)
    {
        return Ok(model.clone());
    }
    DEFAULT_MODELS
        .iter()
        .filter(|(provider, _)| registry.has_api_key(provider))
        .find_map(|(provider, id)| registry.find(provider, id))
        .or_else(|| registry.models().into_iter().find(|model| registry.has_api_key(&model.provider)))
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("No model available. Set a provider API key (for example ANTHROPIC_API_KEY or OPENAI_API_KEY) or configure models.json.")
        })
}

impl CodingSession {
    pub fn new(options: SessionOptions) -> anyhow::Result<Self> {
        let registry = ModelRegistry::load(&models_path(&options.agent_dir))?;
        let settings = Settings::load(&options.agent_dir, &options.cwd);
        let sessions_dir = options
            .persist
            .then(|| options.session_dir.clone().unwrap_or_else(|| session_dir(&options.agent_dir, &options.cwd)));
        let session = match (&options.session_file, options.continue_recent, &sessions_dir) {
            (Some(path), _, _) => SessionManager::open(path)?,
            (None, true, Some(dir)) => match SessionManager::most_recent(dir) {
                Some(path) => SessionManager::open(&path)?,
                None => SessionManager::create(&options.cwd, Some(dir)),
            },
            (None, _, dir) => SessionManager::create(&options.cwd, dir.as_deref()),
        };
        let restored = session.context();
        let model = resolve_model(
            &registry,
            options.provider.as_deref(),
            options.model.as_deref(),
            restored.model.as_ref(),
            &settings,
        )?;
        let thinking =
            options.thinking.or(restored.thinking_level).or(settings.default_thinking_level).unwrap_or_default();
        let thinking = if model.reasoning { thinking } else { ThinkingLevel::Off };

        let mut tool_names: Vec<String> = match &options.tools {
            Some(names) => names.clone(),
            None => tools::DEFAULT_TOOLS.iter().map(|name| name.to_string()).collect(),
        };
        tool_names.retain(|name| !options.exclude_tools.contains(name));
        let tool_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
        let agent_tools = tools::create_tools(&tool_refs, &options.cwd)?;
        let context_files =
            if options.no_context_files { Vec::new() } else { load_context_files(&options.cwd, &options.agent_dir) };
        let system_prompt = build_system_prompt(&SystemPromptOptions {
            custom_prompt: options.system_prompt.as_deref(),
            append_prompt: options.append_system_prompt.as_deref(),
            selected_tools: &tool_names,
            context_files: &context_files,
            cwd: &options.cwd,
        });

        let agent = Agent::new(model.clone());
        agent.update_state(|state| {
            state.system_prompt = Some(system_prompt);
            state.thinking_level = thinking;
            state.tools = agent_tools;
            state.messages = restored.messages.clone();
        });
        let key_registry = registry.clone();
        let api_key = options.api_key.clone();
        let override_key = api_key.clone();
        let override_provider = model.provider.clone();
        agent.set_hooks(AgentHooks {
            get_api_key: Some(Arc::new(move |provider| {
                override_key
                    .clone()
                    .filter(|_| provider == override_provider)
                    .or_else(|| key_registry.api_key(provider))
            })),
            session_id: Some(session.id().to_string()),
            ..AgentHooks::default()
        });

        let session = Arc::new(Mutex::new(session));
        let recorder = session.clone();
        agent.subscribe(move |event| {
            if let AgentEvent::MessageEnd { message } = event
                && let Err(error) = lock(&recorder).append_message(message)
            {
                eprintln!("Warning: could not save session: {error:#}");
            }
        });
        let coding = Self {
            agent,
            session,
            registry,
            api_key,
            compaction: CompactionSettings::default(),
            cwd: options.cwd,
            sessions_dir,
            context_files,
            tool_names,
        };
        if restored.model.as_ref() != Some(&(model.provider.clone(), model.id.clone())) {
            lock(&coding.session).append_model_change(&model.provider, &model.id)?;
        }
        if restored.thinking_level != Some(thinking) {
            lock(&coding.session).append_thinking_level_change(thinking)?;
        }
        coding.refresh_environment();
        Ok(coding)
    }

    /// Publish the session, model, and reasoning level to bash commands as `PI_*` variables.
    fn refresh_environment(&self) {
        let state = self.agent.state();
        let mut variables = std::collections::BTreeMap::new();
        variables.insert("PI_SESSION_ID".to_string(), self.session_id());
        if let Some(file) = self.session_file() {
            variables.insert("PI_SESSION_FILE".to_string(), file.display().to_string());
        }
        variables.insert("PI_PROVIDER".to_string(), state.model.provider.clone());
        variables.insert("PI_MODEL".to_string(), state.model.id.clone());
        variables.insert("PI_REASONING_LEVEL".to_string(), state.thinking_level.as_str().to_string());
        tools::set_session_environment(variables);
    }

    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn model(&self) -> Model {
        self.agent.state().model
    }

    pub fn session_file(&self) -> Option<PathBuf> {
        lock(&self.session).file().map(Path::to_path_buf)
    }

    pub fn session_id(&self) -> String {
        lock(&self.session).id().to_string()
    }

    pub fn sessions_dir(&self) -> Option<&Path> {
        self.sessions_dir.as_deref()
    }

    pub fn api_key_for(&self, provider: &str) -> Option<String> {
        self.api_key.clone().filter(|_| provider == self.model().provider).or_else(|| self.registry.api_key(provider))
    }

    pub fn set_model(&self, model: Model) -> anyhow::Result<()> {
        lock(&self.session).append_model_change(&model.provider, &model.id)?;
        let reasoning = model.reasoning;
        self.agent.update_state(|state| {
            state.model = model;
            if !reasoning {
                state.thinking_level = ThinkingLevel::Off;
            }
        });
        self.refresh_environment();
        Ok(())
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()> {
        let level = if self.model().reasoning { level } else { ThinkingLevel::Off };
        lock(&self.session).append_thinking_level_change(level)?;
        self.agent.update_state(|state| state.thinking_level = level);
        self.refresh_environment();
        Ok(())
    }

    /// Start a fresh session with the same model, tools, and prompt.
    pub fn new_session(&self) -> anyhow::Result<()> {
        let model = self.model();
        let thinking = self.agent.state().thinking_level;
        let mut session = SessionManager::create(&self.cwd, self.sessions_dir.as_deref());
        session.append_model_change(&model.provider, &model.id)?;
        session.append_thinking_level_change(thinking)?;
        self.agent.update_hooks(|hooks| hooks.session_id = Some(session.id().to_string()));
        *lock(&self.session) = session;
        self.agent.update_state(|state| state.messages.clear());
        self.refresh_environment();
        Ok(())
    }

    /// Switch to the session stored in `path`.
    pub fn open_session(&self, path: &Path) -> anyhow::Result<()> {
        let session = SessionManager::open(path)?;
        let context = session.context();
        if let Some(model) = context.model.as_ref().and_then(|(provider, id)| self.registry.find(provider, id)) {
            let model = model.clone();
            self.agent.update_state(|state| state.model = model);
        }
        if let Some(level) = context.thinking_level {
            self.agent.update_state(|state| state.thinking_level = level);
        }
        self.agent.update_hooks(|hooks| hooks.session_id = Some(session.id().to_string()));
        *lock(&self.session) = session;
        self.agent.update_state(|state| state.messages = context.messages);
        self.refresh_environment();
        Ok(())
    }

    /// Run a prompt. Compacts first when the context is near the window, and once
    /// more with a retry when the provider reports a context overflow.
    pub async fn prompt(&self, text: &str, images: Vec<ContentBlock>) -> anyhow::Result<()> {
        self.compact_if_needed().await;
        self.agent.prompt(text, images).await?;
        let state = self.agent.state();
        if let Some(Message::Assistant(last)) = state.messages.last()
            && is_context_overflow(last, Some(state.model.context_window))
            && self.compaction.enabled
        {
            self.agent.update_state(|state| {
                state.messages.pop();
            });
            if self.compact(None).await.is_ok() {
                self.agent.continue_run().await?;
            }
        }
        Ok(())
    }

    async fn compact_if_needed(&self) {
        let state = self.agent.state();
        if should_compact(context_tokens(&state.messages), state.model.context_window, &self.compaction)
            && let Err(error) = self.compact(None).await
        {
            eprintln!("Warning: automatic compaction failed: {error:#}");
        }
    }

    /// Summarize older history now. `instructions` focuses the summary.
    pub async fn compact(&self, instructions: Option<&str>) -> anyhow::Result<CompactionResult> {
        let plan = prepare_compaction(&lock(&self.session), &self.compaction)
            .ok_or_else(|| anyhow::anyhow!("Nothing to compact yet"))?;
        let model = self.model();
        let api_key = self.api_key_for(&model.provider);
        let mut session = lock(&self.session).clone();
        let result = compaction::compact(
            &mut session,
            plan,
            &model,
            api_key,
            instructions,
            &self.compaction,
            CancellationToken::new(),
        )
        .await?;
        let messages = session.context().messages;
        *lock(&self.session) = session;
        self.agent.update_state(|state| state.messages = messages);
        Ok(result)
    }
}
