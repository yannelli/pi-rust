//! Harness options and the services every Harness component shares.

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_chord::Context;

use super::agent::{AGENT_DOC, resolve_agent};
use super::registry::{RegistryReader, RegistrySnapshot};
use super::settings::{Settings, SettingsSource, resolve_settings};
use super::types::{Agent, AgentState};
use crate::env::SharedEnv;
use crate::errors::{DurableError, DurableResult};
use crate::ids::ConversationId;
use crate::models::Models;
use crate::session::{Session, Tx};
use crate::types::ConversationRecord;

/// What `HarnessOptions::env` builds an environment for.
#[derive(Clone)]
pub struct EnvTarget {
    pub conversation_id: ConversationId,
    /// The conversation's agent `cwd`.
    pub cwd: Option<String>,
    /// Committed document reads.
    pub read: Session,
}

/// Builds a conversation's environment at each use; may be async. Never called on the Session line.
pub type EnvBuilder =
    Arc<dyn Fn(EnvTarget, Context) -> BoxFuture<'static, DurableResult<Option<SharedEnv>>> + Send + Sync>;
/// Runs in every commit that creates or forks a conversation, after the built-in documents.
pub type ConversationCreatedHook =
    Arc<dyn Fn(Tx, ConversationRecord) -> BoxFuture<'static, DurableResult<()>> + Send + Sync>;
pub type ReportFn = Arc<dyn Fn(&DurableError) + Send + Sync>;
pub type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Options of `Harness::open`.
#[derive(Clone)]
pub struct HarnessOptions {
    /// pi-ai model access used by generation.
    pub models: Arc<dyn Models>,
    pub registry: Arc<dyn RegistryReader>,
    pub settings: Option<SettingsSource>,
    pub env: Option<EnvBuilder>,
    pub conversation_created: Option<ConversationCreatedHook>,
    /// Wall clock in milliseconds; default the system clock.
    pub now: Option<NowFn>,
    /// Receives failures that do not fail the calling operation, and the error that fails the Session, once.
    pub on_report: Option<ReportFn>,
}

impl HarnessOptions {
    pub fn new(models: Arc<dyn Models>, registry: Arc<dyn RegistryReader>) -> Self {
        Self { models, registry, settings: None, env: None, conversation_created: None, now: None, on_report: None }
    }

    pub fn with_settings(mut self, settings: super::settings::HarnessSettings) -> Self {
        self.settings = Some(super::settings::fixed_settings(settings));
        self
    }

    pub fn with_env(
        mut self,
        env: impl Fn(EnvTarget, Context) -> BoxFuture<'static, DurableResult<Option<SharedEnv>>> + Send + Sync + 'static,
    ) -> Self {
        self.env = Some(Arc::new(env));
        self
    }

    /// An environment builder that returns `env` for every conversation.
    pub fn with_shared_env(self, env: SharedEnv) -> Self {
        self.with_env(move |_, _| {
            let env = env.clone();
            Box::pin(async move { Ok(Some(env)) })
        })
    }

    pub fn with_now(mut self, now: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.now = Some(Arc::new(now));
        self
    }

    pub fn with_on_report(mut self, report: impl Fn(&DurableError) + Send + Sync + 'static) -> Self {
        self.on_report = Some(Arc::new(report));
        self
    }
}

/// The services the Harness components share.
pub(crate) struct HarnessCore {
    pub session: Session,
    pub models: Arc<dyn Models>,
    pub registry: Arc<dyn RegistryReader>,
    pub settings: Option<SettingsSource>,
    pub env: Option<EnvBuilder>,
    pub conversation_created: Option<ConversationCreatedHook>,
    pub now: NowFn,
    pub report: ReportFn,
}

impl HarnessCore {
    pub fn now(&self) -> f64 {
        (self.now)()
    }

    pub fn report(&self, error: &DurableError) {
        (self.report)(error);
    }

    /// The resolved settings. A failing settings source is reported and the defaults apply.
    pub fn settings(&self) -> Settings {
        match &self.settings {
            None => resolve_settings(None),
            Some(source) => match source() {
                Ok(settings) => resolve_settings(Some(&settings)),
                Err(error) => {
                    self.report(&error);
                    resolve_settings(None)
                }
            },
        }
    }

    /// Resolve a conversation's committed `pi.agent` against `snapshot`, or the current one.
    pub async fn resolve_agent(
        &self,
        id: ConversationId,
        snapshot: Option<RegistrySnapshot>,
        context: &Context,
    ) -> DurableResult<Agent> {
        let registry = snapshot.unwrap_or_else(|| self.registry.snapshot());
        let state = self.session.snapshot(&AGENT_DOC, id, context).await?;
        let state: Option<AgentState> = state.map(|value| AGENT_DOC.decode(&value)).transpose()?;
        Ok(resolve_agent(state.as_ref(), &registry, &self.settings(), &|error| self.report(error)))
    }

    /// Build a conversation's environment from its current `cwd`; `None` without an `env` option.
    pub async fn build_env(&self, id: ConversationId, context: &Context) -> DurableResult<Option<SharedEnv>> {
        let Some(build) = &self.env else { return Ok(None) };
        let state = self.session.snapshot(&AGENT_DOC, id, context).await?;
        let cwd = state.and_then(|value| value.get("cwd").and_then(|cwd| cwd.as_str().map(str::to_owned)));
        build(EnvTarget { conversation_id: id, cwd, read: self.session.clone() }, context.clone()).await
    }
}
