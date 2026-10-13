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
}

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

/// Adds entries to a new session before it starts (upstream `newSession({ setup })`).
pub type SessionSetupFn = Box<dyn FnOnce(&mut SessionManager) + Send>;

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
        Some(missing) => anyhow::bail!(missing.error_message()),
        None => Ok(()),
    }
}

pub struct AgentSessionRuntime {
    current: Mutex<RuntimeParts>,
    factory: RuntimeFactory,
    rebind: Mutex<Option<RebindFn>>,
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
        };
        let parts = factory(target).await?;
        Ok(Self { current: Mutex::new(parts), factory, rebind: Mutex::new(None), agent_dir: agent_dir.to_path_buf() })
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

    async fn teardown_current(&self) {
        let session = self.session();
        session.abort().await;
        session.dispose();
    }

    async fn replace(
        &self,
        session_manager: SessionManager,
        reason: SessionStartReason,
        previous_session_file: Option<PathBuf>,
    ) -> anyhow::Result<()> {
        self.teardown_current().await;
        let target = RuntimeTarget {
            cwd: session_manager.cwd().to_path_buf(),
            agent_dir: self.agent_dir.clone(),
            session_manager,
            reason,
            previous_session_file,
        };
        let parts = (self.factory)(target).await?;
        let session = parts.session.clone();
        *lock(&self.current) = parts;
        let rebind = lock(&self.rebind).clone();
        if let Some(rebind) = rebind {
            rebind(&session);
        }
        Ok(())
    }

    /// Open the session stored in `path` (`/resume`).
    pub async fn switch_session(&self, path: &Path, cwd_override: Option<&Path>) -> anyhow::Result<()> {
        let previous = self.session().session_file();
        let session_manager = SessionManager::open_with(path, None, cwd_override)?;
        assert_cwd_exists(&session_manager, &self.cwd())?;
        self.replace(session_manager, SessionStartReason::Resume, previous).await
    }

    /// Start a new session in the same directory (`/new`). `setup` can add entries first.
    pub async fn new_session(
        &self,
        parent_session: Option<String>,
        setup: Option<SessionSetupFn>,
    ) -> anyhow::Result<()> {
        let current = self.session();
        let previous = current.session_file();
        let (persisted, dir) = {
            let manager = current.session_manager();
            (manager.is_persisted(), manager.session_dir().to_path_buf())
        };
        let options = NewSessionOptions { id: None, parent_session };
        let cwd = self.cwd();
        let mut session_manager = SessionManager::create_with(&cwd, persisted.then_some(dir.as_path()), options)?;
        if let Some(setup) = setup {
            setup(&mut session_manager);
        }
        self.replace(session_manager, SessionStartReason::New, previous).await
    }

    /// Fork at `entry_id` into a new session. `Before` a user message returns its text.
    pub async fn fork(&self, entry_id: &str, position: ForkPosition) -> anyhow::Result<ForkResult> {
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
    pub async fn import_from_jsonl(&self, input: &Path, cwd_override: Option<&Path>) -> anyhow::Result<()> {
        let resolved = resolve_path(&crate::config::expand_tilde(&input.to_string_lossy()));
        if !resolved.exists() {
            anyhow::bail!("File not found: {}", resolved.display());
        }
        let current = self.session();
        let previous = current.session_file();
        let dir = current.session_manager().session_dir().to_path_buf();
        let session_manager = import_session_file(&resolved, &dir, cwd_override)?;
        assert_cwd_exists(&session_manager, &self.cwd())?;
        self.replace(session_manager, SessionStartReason::Resume, previous).await
    }

    /// Abort and dispose of the active session.
    pub async fn dispose(&self) {
        self.teardown_current().await;
    }
}
