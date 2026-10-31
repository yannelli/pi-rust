//! Harness options and the services every Harness component shares.

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::{ClientIdentity, merge_client_identity, pi_attribution};
use pi_chord::Context;
use pi_telemetry::TelemetryContext;

use super::agent::{AGENT_DOC, resolve_agent};
use super::registry::{RegistryReader, RegistrySnapshot};
use super::settings::{Settings, SettingsSource, resolve_settings};
use super::telemetry::DurableTelemetry;
use super::types::{Agent, AgentState, Wake};
use crate::env::SharedEnv;
use crate::errors::{DurableError, DurableResult};
use crate::ids::ConversationId;
use crate::models::{Models, with_client_identity};
use crate::session::{Session, Tx};
use crate::timers::{Disarm, SetTimer, TimerCallback, TimerOptions, Timers};
use crate::types::{ConversationRecord, HostCallback, Report, ReportInfo};

/// Request identity of a Harness's model requests (upstream `DEFAULT_HARNESS_IDENTITY`): pi-ai's defaults with
/// provider attribution to pi.
pub fn default_harness_identity() -> ClientIdentity {
    ClientIdentity::attributed(pi_attribution())
}

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
/// Receives one report: the failure and what it is about.
pub type ReportFn = Arc<dyn Fn(&Report) + Send + Sync>;
pub type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;
/// Told when the Session next needs a process (`HarnessOptions::on_wake`); an error is reported.
pub type WakeFn = Arc<dyn Fn(Wake) -> DurableResult<()> + Send + Sync>;

/// Options of `Harness::open`.
#[derive(Clone)]
pub struct HarnessOptions {
    /// Host-owned adapter for content-free admission and phase telemetry (`DURABLE_TELEMETRY_SCHEMA`); its
    /// propagation and metrics capabilities are optional. Default no-op.
    pub telemetry: Option<Arc<dyn TelemetryContext>>,
    /// pi-ai model access used by generation.
    pub models: Arc<dyn Models>,
    /// How model requests identify the app to providers, merged field by field over [`default_harness_identity`].
    /// It applies to every request through the Harness's models, including hooks and tools, and overrides the
    /// identity the `Models` was created with; a request's own `identity` overrides it.
    pub identity: Option<ClientIdentity>,
    pub registry: Arc<dyn RegistryReader>,
    pub settings: Option<SettingsSource>,
    pub env: Option<EnvBuilder>,
    pub conversation_created: Option<ConversationCreatedHook>,
    /// The Harness clock in epoch milliseconds; default the system clock. Sleeping tasks store deadlines on it, so
    /// every process that opens the same storage must use the same clock.
    pub now: Option<NowFn>,
    /// Arms the timers of the scheduler and its tasks: pacing progress commits, freeing idle memory (`background`),
    /// and, without `on_wake`, resuming sleeping tasks; and the timers of `TaskRuntime::set_timer()`. Default a Tokio
    /// timer. Tests can pass fake timers; a host can keep timers off a process that should hibernate.
    pub set_timer: Option<SetTimer>,
    /// Wakes the process for sleeping tasks instead of a `set_timer` timer. Without it, while scheduling runs, a timer
    /// resumes sleeping tasks when they are due. With it, the Harness arms no timer for them: it calls `on_wake` after
    /// open and whenever `Harness::wake()` changes, and the host arms its own wakeup and calls `resume()` when it
    /// fires. Called off the Session line, after the commit that changed the value. It may call `wake()` and
    /// `resume()`, and must not block. An error is reported, and the value is offered again at the next change.
    pub on_wake: Option<WakeFn>,
    /// Receives failures that do not fail the calling operation, such as a failing hook or listener, a task that
    /// faulted, and the error that fails the Session, once: one [`Report`], the error and what happened. A host can tell
    /// a failed Session (`StorageFailed`, often a platform reset) from a faulted task (`TaskFaulted`).
    pub on_report: Option<ReportFn>,
    /// Open for reading only, as a viewer of a session that another process drives, for example a synced copy on a
    /// device that only shows it. Open recovers nothing: a `running` task stays as committed. Every commit with writes
    /// rejects with `ReadOnlySession` before it reaches Storage, and so does `resume()` and every call that enables
    /// scheduling; the Harness stays usable for reads and watches. `wake()` is always inactive, and `on_wake` is never
    /// called. Reopen without it to write.
    pub read_only: bool,
}

impl HarnessOptions {
    pub fn new(models: Arc<dyn Models>, registry: Arc<dyn RegistryReader>) -> Self {
        Self {
            telemetry: None,
            models,
            identity: None,
            registry,
            settings: None,
            env: None,
            conversation_created: None,
            now: None,
            set_timer: None,
            on_wake: None,
            on_report: None,
            read_only: false,
        }
    }

    /// Set [`Self::identity`]. `attribution: Some(None)` sends no attribution headers.
    pub fn with_identity(mut self, identity: ClientIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The Harness's models with [`Self::identity`] applied over [`default_harness_identity`].
    pub(crate) fn identified_models(&self) -> Arc<dyn Models> {
        let default = default_harness_identity();
        let identity = merge_client_identity([Some(&default), self.identity.as_ref()]).unwrap_or(default);
        with_client_identity(self.models.clone(), identity)
    }

    pub fn with_telemetry(mut self, telemetry: Arc<dyn TelemetryContext>) -> Self {
        self.telemetry = Some(telemetry);
        self
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

    pub fn with_set_timer(
        mut self,
        set_timer: impl Fn(TimerCallback, f64, TimerOptions) -> Option<Disarm> + Send + Sync + 'static,
    ) -> Self {
        self.set_timer = Some(Arc::new(set_timer));
        self
    }

    pub fn with_on_wake(mut self, on_wake: impl Fn(Wake) -> DurableResult<()> + Send + Sync + 'static) -> Self {
        self.on_wake = Some(Arc::new(on_wake));
        self
    }

    pub fn with_on_report(mut self, report: impl Fn(&Report) + Send + Sync + 'static) -> Self {
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
    pub now: NowFn,
    pub timers: Timers,
    pub on_wake: Option<WakeFn>,
    pub on_report: Option<ReportFn>,
    pub telemetry: Arc<DurableTelemetry>,
    /// `HarnessOptions::read_only`: open loads live tasks as committed, `running` ones included, and the scheduler
    /// never commits, runs anything, or asks for a wakeup.
    pub read_only: bool,
}

impl HarnessCore {
    pub fn now(&self) -> f64 {
        (self.now)()
    }

    /// Report to `HarnessOptions::on_report`, count it, and add a `pi.durable.report` event to the context's span.
    pub fn report(&self, error: &DurableError, info: &ReportInfo, context: Option<&Context>) {
        if let Some(on_report) = &self.on_report {
            on_report(&Report { info: info.clone(), error: error.clone() });
        }
        self.telemetry.report(info, context);
    }

    /// The resolved settings. A failing settings source is reported and the defaults apply.
    pub fn settings(&self) -> Settings {
        match &self.settings {
            None => resolve_settings(None),
            Some(source) => match source() {
                Ok(settings) => resolve_settings(Some(&settings)),
                Err(error) => {
                    let info = ReportInfo::HostError { callback: HostCallback::Settings, task: None };
                    self.report(&error, &info, None);
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
        let report = |error: &DurableError, extension: &str| {
            let extension = Some(extension.to_owned());
            let info = ReportInfo::ExtensionError { extension, task: None, conversation_id: Some(id) };
            self.report(error, &info, Some(context));
        };
        Ok(resolve_agent(state.as_ref(), &registry, &self.settings(), &report))
    }

    /// Build a conversation's environment from its current `cwd`; `None` without an `env` option.
    pub async fn build_env(&self, id: ConversationId, context: &Context) -> DurableResult<Option<SharedEnv>> {
        let Some(build) = &self.env else { return Ok(None) };
        let state = self.session.snapshot(&AGENT_DOC, id, context).await?;
        let cwd = state.and_then(|value| value.get("cwd").and_then(|cwd| cwd.as_str().map(str::to_owned)));
        build(EnvTarget { conversation_id: id, cwd, read: self.session.clone() }, context.clone()).await
    }
}
