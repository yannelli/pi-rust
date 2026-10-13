//! The session runtime (upstream `agent-session-runtime.ts`): owns the active session and
//! replaces it for `/new`, `/resume`, `/fork`, `/clone`, and `/import`, recreating the
//! cwd-bound services through a factory.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use super::services::{AgentSessionServices, RuntimeDiagnostic};
use super::{AgentSession, lock};
use crate::compaction::utils::user_content_text;
use crate::session::branching::{import_session_file, missing_session_cwd};
use crate::session::ids::resolve_path;
use crate::session::{EntryData, NewSessionOptions, SessionManager};

/// Why a runtime is being created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStartReason {
    Startup,
    New,
    Resume,
    Fork,
}

/// What the factory builds a runtime for.
pub struct RuntimeTarget {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_manager: SessionManager,
    pub reason: SessionStartReason,
    pub previous_session_file: Option<PathBuf>,
    /// Shows the trust prompt for `cwd` when it needs a decision (upstream
    /// `projectTrustContextFactory`); without it the project stays untrusted.
    pub trust_prompt: Option<TrustPromptFn>,
    /// `ctx.ui` of the `project_trust` handlers for `cwd`: the live UI on `/resume`.
    pub trust_ui: Option<TrustUi>,
}

/// An extension UI and mode for `project_trust` handlers (upstream `ProjectTrustContext`).
pub type TrustUi = (Arc<dyn crate::extension_ui::ExtensionUi>, crate::extension_ui::ExtensionMode);

/// Asks the user to pick a project trust option; `None` when dismissed.
pub type TrustPromptFn = Arc<
    dyn Fn(
            crate::resources::trust::ProjectTrustPrompt,
        ) -> BoxFuture<'static, Option<crate::resources::trust::ProjectTrustOption>>
        + Send
        + Sync,
>;

/// A created session with its diagnostics. Its services are `session.services()`.
#[derive(Clone)]
pub struct RuntimeParts {
    pub session: Arc<AgentSession>,
    pub model_fallback_message: Option<String>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

/// Creates services and a session for a target cwd and session manager.
pub type RuntimeFactory = Arc<dyn Fn(RuntimeTarget) -> BoxFuture<'static, anyhow::Result<RuntimeParts>> + Send + Sync>;

/// Called with the new session after every replacement, for example to resubscribe.
pub type RebindFn = Arc<dyn Fn(&Arc<AgentSession>) + Send + Sync>;
/// Runs on the runtime before a session is replaced or disposed.
pub type InvalidateFn = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Adds entries to a new session before it starts (upstream `newSession({ setup })`).
pub type SessionSetupFn = Box<dyn FnOnce(&mut SessionManager) + Send>;

/// Runs on a new session after it is created and before it is bound, as upstream `setup`.
pub type AsyncSetupFn = Box<dyn FnOnce(Arc<AgentSession>) -> BoxFuture<'static, anyhow::Result<()>> + Send>;

/// Whether a `session_before_switch` handler cancelled the switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwitchResult {
    pub cancelled: bool,
}

/// Where a fork starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForkPosition {
    /// Before a user message, which goes back to the editor.
    #[default]
    Before,
    /// At the entry, keeping it (used by `/clone`).
    At,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForkResult {
    pub cancelled: bool,
    /// The text of the user message forked before.
    pub selected_text: Option<String>,
}

fn assert_cwd_exists(session: &SessionManager, fallback: &Path) -> anyhow::Result<()> {
    match missing_session_cwd(session, fallback) {
        Some(missing) => Err(anyhow::Error::new(missing)),
        None => Ok(()),
    }
}

/// `/import` of a file that does not exist.
#[derive(Debug)]
pub struct SessionImportFileNotFound(pub PathBuf);

impl std::fmt::Display for SessionImportFileNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "File not found: {}", self.0.display())
    }
}

impl std::error::Error for SessionImportFileNotFound {}

pub struct AgentSessionRuntime {
    current: Mutex<RuntimeParts>,
    factory: RuntimeFactory,
    rebind: Mutex<Option<RebindFn>>,
    before_invalidate: Mutex<Option<InvalidateFn>>,
    agent_dir: PathBuf,
}

impl AgentSessionRuntime {
    /// Create the first runtime for `session_manager` with `factory`, kept for replacements.
    pub async fn create(
        factory: RuntimeFactory,
        cwd: &Path,
        agent_dir: &Path,
        session_manager: SessionManager,
    ) -> anyhow::Result<Self> {
        assert_cwd_exists(&session_manager, cwd)?;
        let target = RuntimeTarget {
            cwd: session_manager.cwd().to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            session_manager,
            reason: SessionStartReason::Startup,
            previous_session_file: None,
            trust_prompt: None,
            trust_ui: None,
        };
        let parts = factory(target).await?;
        Ok(Self {
            current: Mutex::new(parts),
            factory,
            rebind: Mutex::new(None),
            before_invalidate: Mutex::new(None),
            agent_dir: agent_dir.to_path_buf(),
        })
    }

    pub fn session(&self) -> Arc<AgentSession> {
        lock(&self.current).session.clone()
    }

    pub fn services(&self) -> AgentSessionServices {
        self.session().services().clone()
    }

    pub fn cwd(&self) -> PathBuf {
        self.session().cwd().to_path_buf()
    }

    pub fn diagnostics(&self) -> Vec<RuntimeDiagnostic> {
        lock(&self.current).diagnostics.clone()
    }

    pub fn model_fallback_message(&self) -> Option<String> {
        lock(&self.current).model_fallback_message.clone()
    }

    pub fn set_rebind_session(&self, rebind: Option<RebindFn>) {
        *lock(&self.rebind) = rebind;
    }

    /// Runs after `session_shutdown`, before the session goes away, so a UI can detach what the
    /// old extensions put in it (upstream `setBeforeSessionInvalidate`).
    pub fn set_before_session_invalidate(&self, before: Option<InvalidateFn>) {
        *lock(&self.before_invalidate) = before;
    }

    /// Settle the running turn so it is saved to the outgoing session, send `session_shutdown`,
    /// let the UI detach (upstream `beforeSessionInvalidate`), then invalidate and dispose.
    async fn teardown_current(&self, reason: &str, target: Option<&Path>) {
        let session = self.session();
        let target = target.map(|path| path.display().to_string());
        session.abort().await;
        session.emit_session_shutdown(reason, target.as_deref()).await;
        let before_invalidate = lock(&self.before_invalidate).clone();
        if let Some(before_invalidate) = before_invalidate {
            before_invalidate().await;
        }
        session.invalidate_extensions().await;
        session.shutdown().await;
        // A command that replaced the session still runs on the old runtime (`withSession`); it
        // stops once nothing holds it. Quitting stops it now.
        if let Some(runner) = session.extension_runner() {
            if reason == "quit" { runner.shutdown() } else { runner.restore_apis() }
        }
        session.dispose();
    }

    /// `session_before_switch` or `session_before_fork`: true when a handler cancelled.
    async fn before_replace(&self, event: serde_json::Value) -> bool {
        let Some(runner) = self.session().extension_runner() else { return false };
        runner.emit(event).await["cancel"].as_bool() == Some(true)
    }

    async fn replace(
        &self,
        session_manager: SessionManager,
        reason: SessionStartReason,
        previous_session_file: Option<PathBuf>,
    ) -> anyhow::Result<()> {
        self.replace_with(session_manager, reason, previous_session_file, None, None, None).await
    }

    async fn replace_with(
        &self,
        session_manager: SessionManager,
        reason: SessionStartReason,
        previous_session_file: Option<PathBuf>,
        setup: Option<AsyncSetupFn>,
        trust_prompt: Option<TrustPromptFn>,
        trust_ui: Option<TrustUi>,
    ) -> anyhow::Result<()> {
        let reason_name = match reason {
            SessionStartReason::Startup => "startup",
            SessionStartReason::New => "new",
            SessionStartReason::Resume => "resume",
            SessionStartReason::Fork => "fork",
        };
        let bindings = self.session().extension_bindings();
        let target_file = session_manager.file().map(Path::to_path_buf);
        self.teardown_current(reason_name, target_file.as_deref()).await;
        let target = RuntimeTarget {
            cwd: session_manager.cwd().to_path_buf(),
            agent_dir: self.agent_dir.clone(),
            session_manager,
            reason,
            previous_session_file: previous_session_file.clone(),
            trust_prompt,
            trust_ui,
        };
        let parts = (self.factory)(target).await?;
        let session = parts.session.clone();
        let mut start = serde_json::json!({"type": "session_start", "reason": reason_name});
        if let Some(previous) = &previous_session_file {
            start["previousSessionFile"] = serde_json::json!(previous.display().to_string());
        }
        session.set_session_start_event(start);
        *lock(&self.current) = parts;
        if let Some(setup) = setup {
            setup(session.clone()).await?;
            session.refresh_context();
        }
        // The new session's extensions bind with the mode's bindings before the mode rebinds.
        if let Some(bindings) = bindings {
            session.bind_extensions(bindings).await;
        }
        let rebind = lock(&self.rebind).clone();
        if let Some(rebind) = rebind {
            rebind(&session);
        }
        Ok(())
    }

    /// Open the session stored in `path` (`/resume`).
    pub async fn switch_session(&self, path: &Path, cwd_override: Option<&Path>) -> anyhow::Result<SwitchResult> {
        self.switch_session_with(path, cwd_override, None).await
    }

    /// [`Self::switch_session`] that asks `trust_prompt` when the session's folder needs a
    /// trust decision; the `project_trust` handlers then get the current session's UI.
    pub async fn switch_session_with(
        &self,
        path: &Path,
        cwd_override: Option<&Path>,
        trust_prompt: Option<TrustPromptFn>,
    ) -> anyhow::Result<SwitchResult> {
        let event = serde_json::json!({
            "type": "session_before_switch",
            "reason": "resume",
            "targetSessionFile": path.display().to_string(),
        });
        if self.before_replace(event).await {
            return Ok(SwitchResult { cancelled: true });
        }
        let previous = self.session().session_file();
        let session_manager = SessionManager::open_with(path, None, cwd_override)?;
        assert_cwd_exists(&session_manager, &self.cwd())?;
        let bindings = trust_prompt.as_ref().and_then(|_| self.session().extension_bindings());
        let trust_ui = bindings.map(|bindings| (bindings.ui, bindings.mode));
        let reason = SessionStartReason::Resume;
        self.replace_with(session_manager, reason, previous, None, trust_prompt, trust_ui).await?;
        Ok(SwitchResult::default())
    }

    /// Start a new session in the same directory (`/new`). `setup` can add entries first.
    pub async fn new_session(
        &self,
        parent_session: Option<String>,
        setup: Option<SessionSetupFn>,
    ) -> anyhow::Result<SwitchResult> {
        let setup = setup.map(|setup| -> AsyncSetupFn {
            Box::new(move |session: Arc<AgentSession>| {
                setup(&mut session.session_manager());
                Box::pin(async { Ok(()) })
            })
        });
        self.new_session_with(parent_session, setup).await
    }

    /// [`Self::new_session`] with a setup that runs on the created session.
    pub async fn new_session_with(
        &self,
        parent_session: Option<String>,
        setup: Option<AsyncSetupFn>,
    ) -> anyhow::Result<SwitchResult> {
        if self.before_replace(serde_json::json!({"type": "session_before_switch", "reason": "new"})).await {
            return Ok(SwitchResult { cancelled: true });
        }
        let current = self.session();
        let previous = current.session_file();
        let (persisted, dir) = {
            let manager = current.session_manager();
            (manager.is_persisted(), manager.session_dir().to_path_buf())
        };
        let options = NewSessionOptions { id: None, parent_session };
        let cwd = self.cwd();
        let session_manager = SessionManager::create_with(&cwd, persisted.then_some(dir.as_path()), options)?;
        self.replace_with(session_manager, SessionStartReason::New, previous, setup, None, None).await?;
        Ok(SwitchResult::default())
    }

    /// Fork at `entry_id` into a new session. `Before` a user message returns its text.
    pub async fn fork(&self, entry_id: &str, position: ForkPosition) -> anyhow::Result<ForkResult> {
        let position_name = if position == ForkPosition::At { "at" } else { "before" };
        let event = serde_json::json!({"type": "session_before_fork", "entryId": entry_id, "position": position_name});
        if self.before_replace(event).await {
            return Ok(ForkResult { cancelled: true, selected_text: None });
        }
        let current = self.session();
        let (target_leaf, selected_text) = {
            let manager = current.session_manager();
            let entry = manager.get_entry(entry_id).ok_or_else(|| anyhow::anyhow!("Invalid entry ID for forking"))?;
            match (position, entry.data()) {
                (ForkPosition::At, _) => (Some(entry.id().to_string()), None),
                (ForkPosition::Before, EntryData::Message(pi_ai::Message::User(user))) => {
                    (entry.parent_id().map(String::from), Some(user_content_text(&user.content, "")))
                }
                _ => anyhow::bail!("Invalid entry ID for forking"),
            }
        };
        let previous = current.session_file();
        let (persisted, dir) = {
            let manager = current.session_manager();
            (manager.is_persisted(), manager.session_dir().to_path_buf())
        };
        let cwd = self.cwd();
        let session_manager = if persisted {
            let file =
                previous.clone().ok_or_else(|| anyhow::anyhow!("Persisted session is missing a session file"))?;
            match &target_leaf {
                None => {
                    let options = NewSessionOptions { id: None, parent_session: Some(file.display().to_string()) };
                    SessionManager::create_with(&cwd, Some(&dir), options)?
                }
                Some(leaf) => {
                    if !file.exists() {
                        anyhow::bail!(
                            "This session has not been saved yet. Send a message before cloning or forking it."
                        );
                    }
                    let mut manager = SessionManager::open_with(&file, Some(&dir), None)?;
                    manager.create_branched_session(leaf)?;
                    manager
                }
            }
        } else {
            let mut manager = current.session_manager().clone();
            match &target_leaf {
                None => {
                    let parent_session = previous.as_ref().map(|file| file.display().to_string());
                    manager.new_session(NewSessionOptions { id: None, parent_session })?;
                }
                Some(leaf) => {
                    manager.create_branched_session(leaf)?;
                }
            }
            manager
        };
        self.replace(session_manager, SessionStartReason::Fork, previous).await?;
        Ok(ForkResult { cancelled: false, selected_text })
    }

    /// Copy the current branch into a new session (`/clone`, RPC `clone`).
    pub async fn clone_session(&self) -> anyhow::Result<ForkResult> {
        let leaf = self.session().session_manager().leaf_id().map(String::from);
        let leaf = leaf.ok_or_else(|| anyhow::anyhow!("Cannot clone session: no current entry selected"))?;
        self.fork(&leaf, ForkPosition::At).await
    }

    /// Copy a session JSONL file into the session directory and switch to it (`/import`).
    pub async fn import_from_jsonl(&self, input: &Path, cwd_override: Option<&Path>) -> anyhow::Result<SwitchResult> {
        let resolved = resolve_path(&crate::config::expand_tilde(&input.to_string_lossy()));
        if !resolved.exists() {
            return Err(SessionImportFileNotFound(resolved).into());
        }
        let current = self.session();
        let previous = current.session_file();
        let dir = current.session_manager().session_dir().to_path_buf();
        let destination = dir.join(resolved.file_name().unwrap_or_default());
        let event = serde_json::json!({
            "type": "session_before_switch",
            "reason": "resume",
            "targetSessionFile": destination.display().to_string(),
        });
        if self.before_replace(event).await {
            return Ok(SwitchResult { cancelled: true });
        }
        let session_manager = import_session_file(&resolved, &dir, cwd_override)?;
        assert_cwd_exists(&session_manager, &self.cwd())?;
        self.replace(session_manager, SessionStartReason::Resume, previous).await?;
        Ok(SwitchResult::default())
    }

    /// `session_shutdown` (`quit`), then abort and dispose of the active session.
    pub async fn dispose(&self) {
        self.teardown_current("quit", None).await;
    }
}
