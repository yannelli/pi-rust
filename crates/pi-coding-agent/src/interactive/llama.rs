//! `/llama`: the llama.cpp router view (upstream `extensions/llama` command handler).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::auth::Credential;

use super::mode::InteractiveMode;
use crate::agent_session::ModelRuntime;
use crate::interactive_components::LlamaView;
use crate::llama::{
    HuggingFaceClient, LLAMA_PROVIDER_ID, LlamaCatalogSync, LlamaManager, LlamaProvider, NotifyLevel,
    configured_llama_client, find_hugging_face_token, resolve_llama_auth,
};

/// Rechecks provider auth after the router changed, so the footer and `/model` follow it.
struct RuntimeSync(Arc<ModelRuntime>);

#[async_trait]
impl LlamaCatalogSync for RuntimeSync {
    async fn refresh_llama_models(&self) -> Result<(), String> {
        self.0.refresh_configured_providers().await;
        Ok(())
    }
}

impl InteractiveMode {
    /// `/llama`: list, load, unload, and download router models.
    pub(super) fn handle_llama_command(&self) {
        let runtime = self.session().model_runtime().clone();
        self.spawn(move |mode| async move {
            let store = runtime.credential_store();
            let stored = tokio::spawn(async move { store.read(LLAMA_PROVIDER_ID).await }).await;
            let credential = match stored {
                Ok(Ok(Some(Credential::ApiKey(credential)))) => Some(credential),
                _ => None,
            };
            let env = |name: &str| pi_ai::env::process_env_value(name).filter(|value| !value.is_empty());
            let auth = match resolve_llama_auth(credential.as_ref(), &env) {
                Ok(auth) => auth,
                Err(error) => return mode.show_error(&error),
            };
            let client = match configured_llama_client(auth.as_ref()) {
                Ok(client) => client,
                Err(message) => return mode.show_warning(&message),
            };
            let view = Rc::new(RefCell::new(LlamaView::new(&mode.tui())));
            let close = mode.mount_view(view.clone());
            let notices = mode.weak.clone();
            let ui = LlamaView::bridge(&view, move |message, level| {
                let Some(mode) = notices.upgrade() else { return };
                match level {
                    NotifyLevel::Error => mode.show_error(message),
                    NotifyLevel::Warning => mode.show_warning(message),
                    NotifyLevel::Info => mode.show_status(message),
                }
            });
            let provider = LlamaProvider::new();
            let sync = RuntimeSync(runtime);
            let hugging_face = HuggingFaceClient::new(find_hugging_face_token(), None);
            LlamaManager { client, provider: &provider, ui: &ui, sync: &sync, hugging_face }.run().await;
            close();
            mode.update_available_provider_count();
        });
    }
}
