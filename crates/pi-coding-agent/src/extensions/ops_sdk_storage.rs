//! SDK storage for extensions (upstream `SessionManager` and `SettingsManager` factories): the
//! JavaScript wrappers in `sdk_storage.js` hold ids of the Rust managers kept here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use pi_ai::{Message, UserContent};
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::ops::{Args, error};
use super::ops_sdk_settings;
use super::ops_session::read_manager;
use super::runner::ExtensionRunner;
use crate::agent_session::{AgentSession, lock};
use crate::session::listing::{SessionInfo, list_all_sessions, list_project_sessions};
use crate::session::{FileEntry, NewCompaction, NewSessionOptions, SessionManager, session_dir};
use crate::settings::SettingsManager;

/// A session manager the extension created, one a child session is being created from, or the
/// live manager of the child session that took it.
pub(crate) enum StoredSession {
    Owned(SessionManager),
    Reserved(SessionManager),
    Child(Weak<AgentSession>),
}

#[derive(Default)]
pub(crate) struct StorageState {
    sessions: HashMap<u64, StoredSession>,
    settings: HashMap<u64, Arc<SettingsManager>>,
    next_id: u64,
}

fn disposed(kind: &str) -> JsError {
    error(format!("{kind} has been disposed"))
}

impl StorageState {
    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn insert_session(&mut self, manager: SessionManager) -> u64 {
        let id = self.next();
        self.sessions.insert(id, StoredSession::Owned(manager));
        id
    }

    pub fn insert_settings(&mut self, manager: Arc<SettingsManager>) -> u64 {
        let id = self.next();
        self.settings.insert(id, manager);
        id
    }

    pub fn with_manager<R>(&mut self, id: u64, f: impl FnOnce(&mut SessionManager) -> R) -> Result<R, JsError> {
        match self.sessions.get_mut(&id) {
            Some(StoredSession::Owned(manager)) => Ok(f(manager)),
            Some(StoredSession::Reserved(_)) => Err(error("SessionManager is being attached to an AgentSession")),
            Some(StoredSession::Child(session)) => {
                let session = session.upgrade().ok_or_else(|| disposed("AgentSession"))?;
                let mut manager = session.session_manager();
                Ok(f(&mut manager))
            }
            None => Err(disposed("SessionManager")),
        }
    }

    /// A copy of the manager for a new child session; the entry stays reserved until
    /// [`Self::attach_child`] or [`Self::cancel_create`], so two sessions never share one file.
    pub fn take_session(&mut self, id: u64) -> Result<SessionManager, JsError> {
        let stored = self.sessions.get_mut(&id).ok_or_else(|| disposed("SessionManager"))?;
        match stored {
            StoredSession::Owned(manager) => {
                let copy = manager.clone();
                *stored = StoredSession::Reserved(copy.clone());
                Ok(copy)
            }
            StoredSession::Reserved(_) => Err(error("SessionManager is already being attached to an AgentSession")),
            StoredSession::Child(session) if session.strong_count() > 0 => {
                Err(error("SessionManager is already used by a live AgentSession"))
            }
            StoredSession::Child(_) => Err(disposed("SessionManager")),
        }
    }

    /// Undo [`Self::take_session`] after the child session failed to start.
    pub fn cancel_create(&mut self, id: u64) {
        if let Some(stored) = self.sessions.get_mut(&id)
            && let StoredSession::Reserved(manager) = stored
        {
            *stored = StoredSession::Owned(manager.clone());
        }
    }

    pub fn attach_child(&mut self, id: u64, session: &Arc<AgentSession>) {
        self.sessions.insert(id, StoredSession::Child(Arc::downgrade(session)));
    }

    /// Keep the child's final state after it is disposed, so the wrapper stays usable.
    pub fn detach_child(&mut self, id: u64, session: &Arc<AgentSession>) {
        if let Some(stored @ StoredSession::Child(_)) = self.sessions.get_mut(&id) {
            *stored = StoredSession::Owned(session.session_manager().clone());
        }
    }

    pub fn settings(&self, id: u64) -> Option<Arc<SettingsManager>> {
        self.settings.get(&id).cloned()
    }

    pub(super) fn remove_settings(&mut self, id: u64) {
        self.settings.remove(&id);
    }
}

fn opt_path(args: &Args, index: usize) -> Option<PathBuf> {
    args.opt_str(index).map(PathBuf::from)
}

fn new_session_options(value: &Value) -> NewSessionOptions {
    NewSessionOptions {
        id: value["id"].as_str().map(str::to_string),
        parent_session: value["parentSession"].as_str().map(str::to_string),
    }
}

fn info_json(info: SessionInfo) -> Value {
    json!({
        "path": info.path.display().to_string(),
        "id": info.id,
        "cwd": info.cwd,
        "name": info.name,
        "parentSessionPath": info.parent_session_path,
        "created": info.created,
        "modified": info.modified,
        "messageCount": info.message_count,
        "firstMessage": info.first_message,
        "allMessagesText": info.all_messages_text,
    })
}

fn descriptor(id: u64, manager: &SessionManager, agent_dir: &Path) -> Value {
    json!({
        "id": id,
        "sessionId": manager.id(),
        "cwd": manager.cwd().display().to_string(),
        "sessionDir": manager.session_dir().display().to_string(),
        "sessionFile": manager.file().map(|path| path.display().to_string()),
        "inMemory": !manager.is_persisted(),
        "persisted": manager.is_persisted(),
        "usesDefaultSessionDir": manager.uses_default_session_dir(agent_dir),
    })
}

fn path_json(path: Option<PathBuf>) -> Value {
    json!(path.map(|path| path.display().to_string()))
}

fn json_arg<T: serde::de::DeserializeOwned>(args: &Args, index: usize) -> Result<T, JsError> {
    serde_json::from_value(args.json(index)).map_err(error)
}

fn some_json(args: &Args, index: usize) -> Option<Value> {
    Some(args.json(index)).filter(|value| !value.is_null())
}

/// Mutating `SessionManager` methods; method arguments start at 2.
fn write_manager(manager: &mut SessionManager, method: &str, args: &Args) -> Result<Value, JsError> {
    let id = |result: anyhow::Result<String>| result.map(Value::String).map_err(error);
    match method {
        "appendMessage" => id(manager.append_message(&json_arg::<Message>(args, 2)?)),
        "appendThinkingLevelChange" => {
            let level = crate::model_resolver::parse_thinking_level(&args.str(2)?)
                .ok_or_else(|| error("Invalid thinking level"))?;
            id(manager.append_thinking_level_change(level))
        }
        "appendModelChange" => id(manager.append_model_change(&args.str(2)?, &args.str(3)?)),
        "appendCompaction" => id(manager.append_compaction_with(NewCompaction {
            summary: args.str(2)?,
            first_kept_entry_id: args.opt_str(3),
            tokens_before: args.u64(4),
            details: some_json(args, 5),
            from_hook: args.json(6).as_bool(),
            usage: some_json(args, 7).map(serde_json::from_value).transpose().map_err(error)?,
        })),
        "appendCustomEntry" => id(manager.append_custom_entry(&args.str(2)?, some_json(args, 3))),
        "appendSessionInfo" => id(manager.append_session_info(&args.str(2)?)),
        "appendCustomMessageEntry" => {
            let content: UserContent = json_arg(args, 3)?;
            id(manager.append_custom_message_entry(&args.str(2)?, content, args.bool(4), some_json(args, 5)))
        }
        "appendContextEdit" => id(manager.append_context_edit(&args.str(2)?, some_json(args, 3))),
        "appendLabelChange" => id(manager.append_label_change(&args.str(2)?, args.opt_str(3).as_deref())),
        "branchWithSummary" => {
            let usage = some_json(args, 6).map(serde_json::from_value).transpose().map_err(error)?;
            let from = args.opt_str(2);
            id(manager.branch_with_summary(
                from.as_deref(),
                &args.str(3)?,
                some_json(args, 4),
                args.json(5).as_bool(),
                usage,
            ))
        }
        "branch" => manager.branch(&args.str(2)?).map(|_| Value::Null).map_err(error),
        "resetLeaf" => {
            manager.reset_leaf();
            Ok(Value::Null)
        }
        "newSession" => manager.new_session(new_session_options(&args.json(2))).map(path_json).map_err(error),
        "setSessionFile" => manager.set_session_file(Path::new(&args.str(2)?)).map(|_| Value::Null).map_err(error),
        "createBranchedSession" => manager.create_branched_session(&args.str(2)?).map(path_json).map_err(error),
        other => Err(JsError::unsupported(&format!("SessionManager.{other}"))),
    }
}

fn session_op(runner: &ExtensionRunner, state: &mut StorageState, name: &str, args: &Args) -> Result<Value, JsError> {
    let agent_dir = runner.agent_dir();
    let cwd_or_default = |index: usize| opt_path(args, index).unwrap_or_else(|| runner.cwd().to_path_buf());
    let created = |state: &mut StorageState, manager: anyhow::Result<SessionManager>| {
        manager.map(|manager| json!(state.insert_session(manager))).map_err(error)
    };
    match name {
        "create" => {
            let cwd = cwd_or_default(0);
            let dir = opt_path(args, 1).unwrap_or_else(|| session_dir(agent_dir, &cwd));
            created(state, SessionManager::create_with(&cwd, Some(&dir), new_session_options(&args.json(2))))
        }
        "inMemory" => {
            let entries: Vec<FileEntry> = serde_json::from_value(args.json(2)).unwrap_or_default();
            created(
                state,
                SessionManager::in_memory_with(&cwd_or_default(0), new_session_options(&args.json(1)), entries),
            )
        }
        "open" => {
            let (dir, cwd) = (opt_path(args, 1), opt_path(args, 2));
            created(state, SessionManager::open_with(Path::new(&args.str(0)?), dir.as_deref(), cwd.as_deref()))
        }
        "continueRecent" => {
            let cwd = cwd_or_default(0);
            let dir = opt_path(args, 1).unwrap_or_else(|| session_dir(agent_dir, &cwd));
            created(state, SessionManager::continue_recent(&cwd, &dir, agent_dir))
        }
        "forkFrom" => {
            let (source, cwd, dir) = (args.str(0)?, cwd_or_default(1), opt_path(args, 2));
            let options = new_session_options(&args.json(3));
            created(state, SessionManager::fork_from(Path::new(&source), &cwd, dir.as_deref(), agent_dir, options))
        }
        "list" => {
            let sessions = list_project_sessions(&cwd_or_default(0), opt_path(args, 1).as_deref(), agent_dir);
            Ok(sessions.into_iter().map(info_json).collect())
        }
        "listAll" => {
            Ok(list_all_sessions(opt_path(args, 0).as_deref(), agent_dir).into_iter().map(info_json).collect())
        }
        "findById" => {
            let found =
                SessionManager::find_by_id(&cwd_or_default(0), &args.str(1)?, opt_path(args, 2).as_deref(), agent_dir);
            Ok(path_json(found))
        }
        "descriptor" => {
            let id = args.u64(0);
            state.with_manager(id, |manager| descriptor(id, manager, agent_dir))
        }
        "read" => {
            let method = args.str(1)?;
            state.with_manager(args.u64(0), |manager| read_manager(manager, &method, args))?
        }
        "write" => {
            let method = args.str(1)?;
            state.with_manager(args.u64(0), |manager| write_manager(manager, &method, args))?
        }
        "dispose" => {
            state.sessions.remove(&args.u64(0));
            Ok(Value::Null)
        }
        other => Err(JsError::unsupported(&format!("SessionManager.{other}"))),
    }
}

/// `storage.session.*` and `storage.settings.*` operations.
pub(crate) fn sync(
    runner: &ExtensionRunner,
    state: &Mutex<StorageState>,
    op: &str,
    args: &Args,
) -> Result<HostValue, JsError> {
    let rest = op.strip_prefix("storage.").unwrap_or(op);
    let (group, name) = rest.split_once('.').unwrap_or((rest, ""));
    let mut state = lock(state);
    let value = match group {
        "session" => session_op(runner, &mut state, name, args)?,
        "settings" => ops_sdk_settings::settings_op(runner, &mut state, name, args)?,
        _ => return Err(error(format!("Unknown host operation: {op}"))),
    };
    Ok(HostValue::Json(value))
}
