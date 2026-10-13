//! The ACP agent (upstream `PiAcpAgent`): session lifecycle, listing, configuration, and
//! cancellation. Prompts are in `agent_prompt.rs`, `session/load` in `agent_load.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_client_protocol_schema::{
    AgentCapabilities, AuthenticateResponse, AvailableCommandsUpdate, CancelNotification, DeleteSessionRequest,
    DeleteSessionResponse, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, McpCapabilities, NewSessionRequest, PromptCapabilities, ProtocolVersion, SessionCapabilities,
    SessionDeleteCapabilities, SessionInfo, SessionListCapabilities, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::auth::{TerminalLaunch, auth_methods, client_supports_terminal_auth};
use super::backend::{AcpClient, BackendFactory, PiBackend};
use super::commands::{builtin_available_commands, commands_from_pi, load_slash_commands, merge_commands};
use super::config::{
    MODEL_CONFIG_ID, THOUGHT_LEVEL_CONFIG_ID, emit_config_options_update, session_configuration, set_session_model,
};
use super::errors::{AcpResult, auth_or_internal, auth_required, internal_failure, invalid_params};
use super::listing::{find_pi_session, list_pi_sessions, read_session_title};
use super::session::{AcpSession, AcpSessionParams};
use super::settings::{enable_skill_commands, quiet_startup};
use super::startup::{StartupInfoInput, build_startup_info, update_notice};
use super::store::SessionStore;
use crate::agent_session::lock;
use crate::platform::version_check::LatestPiRelease;
use crate::session::ids::uuid_v4;

const PAGE_SIZE: usize = 50;
const UPDATE_CHECK_WAIT: Duration = Duration::from_millis(800);

/// A response plus work that must run after it was sent, such as notifications the client
/// would drop for a session it does not know yet.
pub struct Handled<T> {
    pub response: T,
    pub after: Option<BoxFuture<'static, ()>>,
}

/// Where the agent finds pi's files and what it advertises.
#[derive(Clone)]
pub struct AcpAgentOptions {
    pub agent_dir: PathBuf,
    pub home: PathBuf,
    /// Where pi's session files are, for listing and discovery.
    pub sessions_root: PathBuf,
    pub session_map_path: PathBuf,
    /// `PI_ACP_ENABLE_EMBEDDED_CONTEXT=true` advertises `embeddedContext`.
    pub embedded_context: bool,
    pub terminal_launch: TerminalLaunch,
    pub version: String,
}

/// `PI_ACP_ENABLE_EMBEDDED_CONTEXT`: only the exact value `true` enables it.
pub fn embedded_context_enabled(value: Option<&str>) -> bool {
    value == Some("true")
}

enum VersionCheck {
    Pending(JoinHandle<Option<LatestPiRelease>>),
    Done(Option<String>),
}

pub struct AcpAgent {
    client: Arc<dyn AcpClient>,
    factory: Arc<dyn BackendFactory>,
    pub(super) options: AcpAgentOptions,
    pub(super) store: SessionStore,
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    last_session_cwd: Mutex<Option<String>>,
    restore_lock: tokio::sync::Mutex<()>,
    version_check: tokio::sync::Mutex<VersionCheck>,
    weak: Weak<AcpAgent>,
}

impl AcpAgent {
    pub fn new(
        client: Arc<dyn AcpClient>,
        factory: Arc<dyn BackendFactory>,
        options: AcpAgentOptions,
        version_check: Option<JoinHandle<Option<LatestPiRelease>>>,
    ) -> Arc<Self> {
        let store = SessionStore::new(options.session_map_path.clone());
        let check = version_check.map_or(VersionCheck::Done(None), VersionCheck::Pending);
        Arc::new_cyclic(|weak| Self {
            client,
            factory,
            options,
            store,
            sessions: Mutex::new(HashMap::new()),
            last_session_cwd: Mutex::new(None),
            restore_lock: tokio::sync::Mutex::new(()),
            version_check: tokio::sync::Mutex::new(check),
            weak: weak.clone(),
        })
    }

    pub(super) fn this(&self) -> Arc<Self> {
        self.weak.upgrade().expect("the agent outlives its calls")
    }

    pub(super) fn client(&self) -> &Arc<dyn AcpClient> {
        &self.client
    }

    pub fn session(&self, session_id: &str) -> Option<Arc<AcpSession>> {
        lock(&self.sessions).get(session_id).cloned()
    }

    /// Register a session built elsewhere (tests stand in for `session/new`).
    #[cfg(test)]
    pub(super) fn adopt(&self, session: Arc<AcpSession>) {
        lock(&self.sessions).insert(session.session_id.clone(), session);
    }

    pub(super) fn set_last_cwd(&self, cwd: &str) {
        *lock(&self.last_session_cwd) = Some(cwd.to_string());
    }

    /// Stop a session and forget it.
    pub async fn close(&self, session_id: &str) {
        let session = lock(&self.sessions).remove(session_id);
        if let Some(session) = session {
            session.dispose().await;
        }
    }

    /// One live pi session per connection: stop all others.
    pub(super) async fn close_all_except(&self, keep: &str) {
        let others: Vec<Arc<AcpSession>> = {
            let mut sessions = lock(&self.sessions);
            let ids: Vec<String> = sessions.keys().filter(|id| *id != keep).cloned().collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        for session in others {
            session.dispose().await;
        }
    }

    /// Cancel every running prompt (the client went away).
    pub async fn cancel_all(&self) {
        let all: Vec<Arc<AcpSession>> = lock(&self.sessions).values().cloned().collect();
        for session in all {
            session.cancel().await;
        }
    }

    /// Stop every session (end of the connection).
    pub async fn dispose(&self) {
        let all: Vec<Arc<AcpSession>> = lock(&self.sessions).drain().map(|(_, session)| session).collect();
        for session in all {
            session.dispose().await;
        }
    }

    fn register(&self, session_id: &str, cwd: &str, backend: Arc<dyn PiBackend>, auto_title: bool) -> Arc<AcpSession> {
        let session = AcpSession::new(AcpSessionParams {
            session_id: session_id.to_string(),
            cwd: PathBuf::from(cwd),
            backend,
            client: self.client.clone(),
            auto_title,
        });
        session.start_event_pump();
        lock(&self.sessions).insert(session_id.to_string(), session.clone());
        session
    }

    async fn update_notice(&self) -> Option<String> {
        let mut check = self.version_check.lock().await;
        if let VersionCheck::Pending(handle) = &mut *check {
            match tokio::time::timeout(UPDATE_CHECK_WAIT, handle).await {
                Ok(result) => {
                    let release = result.ok().flatten();
                    *check = VersionCheck::Done(update_notice(release.as_ref(), &self.options.version));
                }
                Err(_) => return None,
            }
        }
        match &*check {
            VersionCheck::Done(notice) => notice.clone(),
            VersionCheck::Pending(_) => None,
        }
    }

    pub fn initialize(&self, params: InitializeRequest) -> InitializeResponse {
        let supports_meta = client_supports_terminal_auth(params.client_capabilities.meta.as_ref());
        let prompt = PromptCapabilities::new().image(true).audio(false).embedded_context(self.options.embedded_context);
        let sessions =
            SessionCapabilities::new().list(SessionListCapabilities::new()).delete(SessionDeleteCapabilities::new());
        let capabilities = AgentCapabilities::new()
            .load_session(true)
            .mcp_capabilities(McpCapabilities::new().http(false).sse(false))
            .prompt_capabilities(prompt)
            .session_capabilities(sessions);
        InitializeResponse::new(ProtocolVersion::V1)
            .agent_info(Implementation::new("pi", self.options.version.clone()).title("pi"))
            .auth_methods(auth_methods(supports_meta, &self.options.terminal_launch))
            .agent_capabilities(capabilities)
    }

    /// Terminal Auth happens out of band, so `authenticate` succeeds without doing anything.
    pub fn authenticate(&self) -> AuthenticateResponse {
        AuthenticateResponse::new()
    }

    async fn cleanup_failed_new_session(&self, session_id: &str, state: Option<&Value>) {
        self.close(session_id).await;
        let file = state
            .and_then(|state| state["sessionFile"].as_str())
            .filter(|file| !file.trim().is_empty())
            .map(String::from)
            .or_else(|| self.store.get(session_id).map(|stored| stored.session_file));
        if let Some(file) = file.filter(|file| !file.trim().is_empty()) {
            let _ = std::fs::remove_file(file);
        }
        self.store.delete(session_id);
    }

    pub async fn new_session(&self, params: NewSessionRequest) -> AcpResult<Handled<Value>> {
        if !params.cwd.is_absolute() {
            return Err(invalid_params(format!("cwd must be an absolute path: {}", params.cwd.display())));
        }
        let cwd = params.cwd.display().to_string();
        self.set_last_cwd(&cwd);
        let backend = self.factory.create(&params.cwd, None).await?;
        let first_state = backend.get_state().await.ok();
        let session_id = first_state.as_ref().and_then(|state| state["sessionId"].as_str()).map(String::from);
        let session_id = session_id.unwrap_or_else(uuid_v4);
        if let Some(file) = first_state.as_ref().and_then(|state| state["sessionFile"].as_str()) {
            self.store.upsert(&session_id, &cwd, file);
        }
        let session = self.register(&session_id, &cwd, backend.clone(), true);
        let (state, models) = tokio::join!(backend.get_state(), backend.get_available_models());
        let known_state = state.as_ref().ok().or(first_state.as_ref());
        let models = match models {
            Ok(models) => models,
            Err(error) => {
                self.cleanup_failed_new_session(&session_id, known_state).await;
                return Err(auth_or_internal(&error));
            }
        };
        if models["models"].as_array().is_none_or(Vec::is_empty) {
            self.cleanup_failed_new_session(&session_id, known_state).await;
            return Err(auth_required());
        }
        let state = match state {
            Ok(state) => state,
            Err(error) => {
                self.cleanup_failed_new_session(&session_id, first_state.as_ref()).await;
                return Err(auth_or_internal(&error));
            }
        };
        let configuration = match session_configuration(backend.as_ref(), Some(state.clone()), Some(models)).await {
            Ok(configuration) => configuration,
            Err(error) => {
                self.cleanup_failed_new_session(&session_id, Some(&state)).await;
                return Err(auth_or_internal(&error));
            }
        };
        let notice = self.update_notice().await;
        let prelude = if quiet_startup(&params.cwd, &self.options.agent_dir) {
            notice.map(|notice| notice + "\n").unwrap_or_default()
        } else {
            build_startup_info(&StartupInfoInput {
                cwd: &params.cwd,
                agent_dir: &self.options.agent_dir,
                home: &self.options.home,
                version: &self.options.version,
                update_notice: notice.as_deref(),
            })
        };
        if !prelude.is_empty() {
            session.set_startup_info(prelude.clone());
        }
        self.close_all_except(&session_id).await;
        let response = json!({
            "sessionId": session_id,
            "configOptions": configuration.config_options,
            "models": configuration.models,
            "modes": configuration.modes,
            "_meta": {"piAcp": {"startupInfo": if prelude.is_empty() { Value::Null } else { json!(prelude) }}},
        });
        let (agent, cwd) = (self.this(), params.cwd.clone());
        let after = async move {
            session.send_startup_info_if_pending();
            agent.advertise_commands(&session, &cwd).await;
        };
        Ok(Handled { response, after: Some(Box::pin(after)) })
    }

    /// After `session/new` and `session/load`: context usage, then `available_commands_update`.
    pub(super) async fn advertise_commands(&self, session: &AcpSession, cwd: &Path) {
        session.publish_context_usage().await;
        let skills = enable_skill_commands(cwd, &self.options.agent_dir);
        let commands = match session.backend.get_commands().await {
            Ok(data) => commands_from_pi(&data, skills, false),
            Err(_) => super::commands::to_available_commands(&load_slash_commands(cwd, &self.options.agent_dir)),
        };
        let commands = merge_commands(commands, builtin_available_commands());
        let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands));
        self.client.session_update(SessionNotification::new(session.session_id.clone(), update));
    }

    /// The stored cwd and file of a session, from the map or by scanning pi's sessions.
    pub(super) fn find_stored_session(&self, session_id: &str) -> Option<(String, String)> {
        if let Some(stored) = self.store.get(session_id).filter(|s| !s.cwd.is_empty() && !s.session_file.is_empty()) {
            return Some((stored.cwd, stored.session_file));
        }
        let found = find_pi_session(&self.options.sessions_root, session_id)?;
        let file = found.session_file.display().to_string();
        self.store.upsert(session_id, &found.cwd, &file);
        Some((found.cwd, file))
    }

    /// The live session, or the stored one reopened.
    pub async fn restore_session(&self, session_id: &str, cwd: Option<&str>) -> AcpResult<Arc<AcpSession>> {
        if let Some(session) = self.session(session_id) {
            return Ok(session);
        }
        let _restoring = self.restore_lock.lock().await;
        if let Some(session) = self.session(session_id) {
            return Ok(session);
        }
        let Some((stored_cwd, file)) = self.find_stored_session(session_id) else {
            return Err(invalid_params(format!("Unknown sessionId: {session_id}")));
        };
        let cwd = cwd.map(String::from).unwrap_or(stored_cwd);
        let backend = self.factory.create(Path::new(&cwd), Some(Path::new(&file))).await?;
        let session = self.register(session_id, &cwd, backend, false);
        self.set_last_cwd(&cwd);
        self.store.upsert(session_id, &cwd, &file);
        if let Some(title) = read_session_title(Path::new(&file), None) {
            session.publish_title(Some(title), None);
        }
        Ok(session)
    }

    pub async fn cancel(&self, params: CancelNotification) {
        if let Some(session) = self.session(&params.session_id.0) {
            session.cancel().await;
        }
    }

    /// Sessions in the requested cwd, or the last session's cwd, 50 per page.
    pub fn list_sessions(&self, params: ListSessionsRequest) -> ListSessionsResponse {
        let all = list_pi_sessions(&self.options.sessions_root);
        let cwd = params.cwd.map(|cwd| cwd.display().to_string()).or_else(|| lock(&self.last_session_cwd).clone());
        let filtered: Vec<_> = all.into_iter().filter(|item| cwd.as_ref().is_none_or(|cwd| &item.cwd == cwd)).collect();
        let start = params.cursor.and_then(|cursor| cursor.trim().parse::<usize>().ok()).unwrap_or(0);
        let sessions = filtered
            .iter()
            .skip(start)
            .take(PAGE_SIZE)
            .map(|item| {
                SessionInfo::new(item.session_id.clone(), PathBuf::from(&item.cwd))
                    .title(item.title.clone())
                    .updated_at(item.updated_at.clone())
            })
            .collect();
        let next = (start + PAGE_SIZE < filtered.len()).then(|| (start + PAGE_SIZE).to_string());
        ListSessionsResponse::new(sessions).next_cursor(next).meta(serde_json::Map::new())
    }

    /// Delete the session file and its map entry; unknown sessions succeed too.
    pub fn delete_session(&self, params: DeleteSessionRequest) -> DeleteSessionResponse {
        let id = params.session_id.0.to_string();
        let stored = self.store.get(&id);
        let found = find_pi_session(&self.options.sessions_root, &id);
        if stored.is_none() && found.is_none() {
            return DeleteSessionResponse::new();
        }
        let file = stored.map(|stored| PathBuf::from(stored.session_file)).or(found.map(|found| found.session_file));
        if let Some(file) = file {
            let _ = std::fs::remove_file(file);
        }
        self.store.delete(&id);
        DeleteSessionResponse::new()
    }

    /// Legacy `session/set_mode`: the mode is the thinking level.
    pub async fn set_session_mode(&self, params: SetSessionModeRequest) -> AcpResult<SetSessionModeResponse> {
        let session = self.restore_session(&params.session_id.0, None).await?;
        let mode = params.mode_id.0.to_string();
        if mode.is_empty() {
            return Err(invalid_params("Expected nonempty string modeId"));
        }
        session.backend.set_thinking_level(&mode).await.map_err(|error| internal_failure(&error))?;
        emit_config_options_update(self.client.as_ref(), &session.session_id, session.backend.as_ref())
            .await
            .map_err(|error| internal_failure(&error))?;
        Ok(SetSessionModeResponse::new())
    }

    pub async fn set_session_config_option(
        &self,
        params: SetSessionConfigOptionRequest,
    ) -> AcpResult<SetSessionConfigOptionResponse> {
        let session = self.restore_session(&params.session_id.0, None).await?;
        let config_id = params.config_id.0.to_string();
        let value = params.value.0.to_string();
        let backend = session.backend.as_ref();
        let model_changed = match config_id.as_str() {
            MODEL_CONFIG_ID => {
                set_session_model(backend, &value).await?;
                true
            }
            THOUGHT_LEVEL_CONFIG_ID => {
                if value.is_empty() {
                    return Err(invalid_params("Expected nonempty thinking level"));
                }
                backend.set_thinking_level(&value).await.map_err(|error| internal_failure(&error))?;
                false
            }
            _ => return Err(invalid_params(format!("Unknown config option: {config_id}"))),
        };
        let options = emit_config_options_update(self.client.as_ref(), &session.session_id, backend)
            .await
            .map_err(|error| internal_failure(&error))?;
        if model_changed {
            session.publish_context_usage().await;
        }
        Ok(SetSessionConfigOptionResponse::new(options))
    }

    /// Unstable `session/set_model` (`{sessionId, modelId}`).
    pub async fn set_session_model(&self, session_id: &str, model_id: &str) -> AcpResult<Value> {
        let session = self.restore_session(session_id, None).await?;
        set_session_model(session.backend.as_ref(), model_id).await?;
        emit_config_options_update(self.client.as_ref(), &session.session_id, session.backend.as_ref())
            .await
            .map_err(|error| internal_failure(&error))?;
        session.publish_context_usage().await;
        Ok(json!({}))
    }
}
