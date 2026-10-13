//! `/llama`: the llama.cpp router view (upstream `extensions/llama` command handler).

use std::cell::RefCell;
use std::rc::Rc;

use super::mode::InteractiveMode;
use crate::interactive_components::LlamaView;
use crate::llama::NotifyLevel;

impl InteractiveMode {
    /// `/llama`: list, load, unload, and download router models.
    pub(super) fn handle_llama_command(&self) {
        let session = self.session();
        self.spawn(move |mode| async move {
            let client = session.clone();
            if let Err(message) =
                tokio::spawn(async move { client.llama_client().await.map(drop) }).await.unwrap_or(Ok(()))
            {
                return mode.show_warning(&message);
            }
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
            let result = tokio::spawn(async move { session.run_llama(&ui).await }).await;
            close();
            if let Ok(Err(message)) = result {
                mode.show_warning(&message);
            }
            mode.update_available_provider_count();
        });
    }
}
