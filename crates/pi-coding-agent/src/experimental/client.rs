//! Port of upstream `experimental/client.ts`: the non-interactive client.

use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult};

use super::cli::{ClientCommand, TransportAddress};
use super::client_runtime::{
    ActivatedClientRuntimeServer, OpenClientRuntimeOptions, activate_builtin_client_services, open_client_runtime,
};
use super::services::agent_controller::{AgentController, AgentPromptRequest};
use super::services::plugins::{PrepareSessionRequest, PresentationPlugins};
use super::services::sessions::{SessionCreateOptions, SessionManagement, session_directory_state};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAddress {
    pub server_id: String,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientResult {
    List { sessions: Vec<SessionAddress> },
    Attached { server_id: String, session_id: String },
    Prompted { server_id: String, session_id: String, text: String },
}

/// Upstream `path.resolve` of a plugin package path.
pub fn resolve_path(path: &str) -> String {
    let path = std::path::Path::new(path);
    let absolute = match path.is_absolute() {
        true => path.to_path_buf(),
        false => std::env::current_dir().unwrap_or_default().join(path),
    };
    let mut resolved = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => drop(resolved.pop()),
            std::path::Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved.display().to_string()
}

fn session_ids(server: &ActivatedClientRuntimeServer) -> ChordResult<Vec<String>> {
    let state = session_directory_state(&server.directory)?;
    Ok(state.map(|state| state.sessions.into_iter().map(|session| session.session_id).collect()).unwrap_or_default())
}

/// Discover servers, then list Sessions, attach to one, or create one for a prompt.
pub async fn run_client(command: &ClientCommand, options: OpenClientRuntimeOptions) -> ChordResult<ClientResult> {
    let runtime = open_client_runtime(command, options).await?;
    let result = run(command, &runtime.servers).await;
    let disposed = runtime.dispose().await;
    let result = result?;
    disposed?;
    Ok(result)
}

async fn run(
    command: &ClientCommand,
    servers: &[super::client_runtime::ClientRuntimeServer],
) -> ChordResult<ClientResult> {
    let mut discovered = Vec::new();
    for server in servers {
        discovered.push(activate_builtin_client_services(server).await?);
    }
    if command.session_id.is_none() && command.prompt.is_none() {
        let mut sessions = Vec::new();
        for server in &discovered {
            let server_id = server.server.route.server_id().to_string();
            sessions.extend(
                session_ids(server)?
                    .into_iter()
                    .map(|session_id| SessionAddress { server_id: server_id.clone(), session_id }),
            );
        }
        sessions.sort_by(|left, right| {
            left.server_id.cmp(&right.server_id).then_with(|| left.session_id.cmp(&right.session_id))
        });
        return Ok(ClientResult::List { sessions });
    }
    let (selected, session_id) = match &command.session_id {
        None => {
            let [only] = discovered.as_slice() else {
                return Err(ChordError::error(
                    "Client prompt requires exactly one discovered server to create a Session",
                ));
            };
            let created = only.management.create(SessionCreateOptions::default(), &BACKGROUND_CONTEXT).await?;
            (only, created.session_id)
        }
        Some(session_id) => {
            let mut matches = Vec::new();
            for server in &discovered {
                if session_ids(server)?.contains(session_id) {
                    matches.push(server);
                }
            }
            if matches.len() > 1 {
                return Err(ChordError::error(format!("Session {session_id} is available from more than one server")));
            }
            match (matches.first(), discovered.as_slice()) {
                (Some(existing), _) => (*existing, session_id.clone()),
                (None, [only])
                    if command.prompt.is_some()
                        && !matches!(command.connect, Some(TransportAddress::Radius { .. })) =>
                {
                    let options = SessionCreateOptions { id: Some(session_id.clone()) };
                    only.management.create(options, &BACKGROUND_CONTEXT).await?;
                    (only, session_id.clone())
                }
                _ => return Err(ChordError::error(format!("No discovered server contains session {session_id}"))),
            }
        }
    };
    let package_paths =
        command.plugin_packages.as_ref().map(|paths| paths.iter().map(|path| resolve_path(path)).collect());
    let request = PrepareSessionRequest { session_id: session_id.clone(), package_paths };
    selected.plugins.prepare_session(request, &BACKGROUND_CONTEXT).await?;
    selected.management.attach(session_id.clone(), &BACKGROUND_CONTEXT).await?;
    let server_id = selected.server.route.server_id().to_string();
    let Some(prompt) = &command.prompt else {
        return Ok(ClientResult::Attached { server_id, session_id });
    };
    let response = selected.agent.prompt(AgentPromptRequest::text(prompt), &BACKGROUND_CONTEXT).await?;
    let operation_id = match (response.accepted, response.operation_id, response.error) {
        (true, Some(operation_id), _) => operation_id,
        (_, _, error) => {
            return Err(ChordError::error(
                error.map(|error| error.message).unwrap_or_else(|| "Prompt was rejected".into()),
            ));
        }
    };
    let result = selected.agent.wait_for_prompt(operation_id, &BACKGROUND_CONTEXT).await?;
    if result.status == "unanswered" {
        let reason = result.reason.unwrap_or_default();
        return Err(ChordError::error(format!("Prompt was not answered: {reason}")));
    }
    Ok(ClientResult::Prompted { server_id, session_id, text: result.text.unwrap_or_default() })
}
