//! Provider registration after `/acp` changes the `acpAgents` settings.

/// Registers the enabled ACP agents as providers.
pub async fn sync_providers(session: &crate::agent_session::AgentSession) {
    let agent_dir = session.services().agent_dir.clone();
    super::register::sync(session.model_runtime(), session.settings(), &agent_dir);
}
