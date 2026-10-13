//! `/share` (upstream `session-share.ts`): upload the branch JSONL to Radius when the `radius`
//! provider has a credential, otherwise publish the HTML export as a secret gist with `gh`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use pi_ai::auth::AuthOverrides;
use pi_tui::terminal_image::hyperlink;
use tokio_util::sync::CancellationToken;

use super::components::bordered_loader::BorderedLoader;
use super::mode::InteractiveMode;
use crate::export::share::{
    DEFAULT_RADIUS_GATEWAY, ShareOutcome, SystemCommandRunner, check_gh_auth, create_gist, export_session_for_share,
    upload_radius_artifact,
};

/// Removes the temp directory of the share files when dropped.
struct ShareDir(PathBuf);

impl Drop for ShareDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl InteractiveMode {
    /// `/share`: Radius when signed in, else a secret GitHub gist of the HTML export.
    pub(super) fn handle_share_command(&self) {
        let session = self.session();
        let dir = ShareDir(std::env::temp_dir().join(format!("pi-share-{}", crate::session::ids::uuid_v4())));
        let jsonl = dir.0.join("session.jsonl");
        let exported = std::fs::create_dir_all(&dir.0).map_err(anyhow::Error::from).and_then(|()| {
            let file = session.session_file().ok_or_else(|| anyhow::anyhow!("Cannot export in-memory session"))?;
            export_session_for_share(&file, &jsonl, &session.system_prompt(), &session.export_tools())
        });
        if let Err(error) = exported {
            return self.show_error(&format!("Failed to export session: {error}"));
        }
        self.spawn(move |mode| async move {
            if let Some(token) = mode.radius_share_token().await {
                return mode.share_via_radius(&jsonl, token).await;
            }
            if let Err(error) = check_gh_auth(&SystemCommandRunner).await {
                return mode.show_error(&error.to_string());
            }
            let html = dir.0.join("session.html");
            let theme_name = crate::theme::current_theme_name();
            if let Err(error) =
                mode.session().export_to_html_themed(Some(&html.to_string_lossy()), theme_name.as_deref())
            {
                return mode.show_error(&format!("Failed to export session: {error}"));
            }
            mode.share_via_gist(&html).await;
            drop(dir);
        });
    }

    /// The `radius` provider's token, valid for five more minutes, when the provider exists.
    async fn radius_share_token(&self) -> Option<String> {
        let runtime = self.session().model_runtime().clone();
        if !runtime.registry().has_provider("radius") {
            return None;
        }
        let overrides = AuthOverrides { min_oauth_validity_ms: Some(5 * 60_000), ..Default::default() };
        let resolve = async move { runtime.resolver().resolve("radius", &overrides).await.ok().flatten() };
        tokio::spawn(resolve).await.ok().flatten().and_then(|resolved| resolved.auth.api_key)
    }

    /// A loader in the editor slot; aborting it restores the editor and reports the cancel.
    fn show_share_loader(&self, message: &str) -> (Rc<RefCell<BorderedLoader>>, CancellationToken) {
        let loader = Rc::new(RefCell::new(BorderedLoader::new(&self.tui(), message, true)));
        let cancel = loader.borrow().signal();
        let weak = self.weak.clone();
        loader.borrow_mut().set_on_abort(Some(Box::new(move || {
            if let Some(mode) = weak.upgrade() {
                mode.selector.restore_editor();
                mode.show_status("Share cancelled");
            }
        })));
        self.show_in_editor_slot(loader.clone(), loader.clone());
        (loader, cancel)
    }

    async fn share_via_radius(&self, jsonl: &std::path::Path, token: String) {
        let (loader, cancel) = self.show_share_loader("Uploading to Radius...");
        let body = std::fs::read(jsonl).unwrap_or_default();
        let upload = async move {
            let client = pi_ai::http::shared_client();
            upload_radius_artifact(&client, DEFAULT_RADIUS_GATEWAY, &token, body).await
        };
        let task = tokio::spawn(upload);
        let result = tokio::select! {
            result = task => result,
            () = cancel.cancelled() => return,
        };
        loader.borrow_mut().dispose();
        self.selector.restore_editor();
        match result {
            Ok(Ok(url)) => self.show_status(&format!("Share URL: {}", hyperlink(&url, &url))),
            Ok(Err(error)) => self.show_error(&error.to_string()),
            Err(error) => self.show_error(&format!("Failed to upload Radius artifact: {error}")),
        }
    }

    async fn share_via_gist(&self, html: &std::path::Path) {
        let (loader, cancel) = self.show_share_loader("Creating gist...");
        let result = tokio::select! {
            result = create_gist(&SystemCommandRunner, html) => result,
            () = cancel.cancelled() => return,
        };
        loader.borrow_mut().dispose();
        self.selector.restore_editor();
        match result {
            Ok(ShareOutcome::Gist { gist_url, preview_url }) => {
                let preview = hyperlink(&preview_url, &preview_url);
                self.show_status(&format!("Share URL: {preview}\nGist: {}", hyperlink(&gist_url, &gist_url)));
            }
            Ok(ShareOutcome::Radius { url }) => self.show_status(&format!("Share URL: {}", hyperlink(&url, &url))),
            Err(error) => self.show_error(&error.to_string()),
        }
    }
}
