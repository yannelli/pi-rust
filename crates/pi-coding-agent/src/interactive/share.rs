//! `/share` (upstream `session-share.ts`): upload the HTML export to the user's pi-rust.com
//! account (pi-rust extension), the branch JSONL to Radius when the `radius` provider has a
//! credential, or publish the HTML export as a secret gist with `gh`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use pi_ai::auth::AuthOverrides;
use pi_tui::program_status::BlockedKind;
use pi_tui::terminal_image::hyperlink;
use tokio_util::sync::CancellationToken;

use super::components::bordered_loader::BorderedLoader;
use super::mode::InteractiveMode;
use crate::export::pi_rust_share::{
    ACCOUNT_LABEL, PiRustAccount, PiRustError, ShareOptions, delete_account, load_account_for, revoke_token,
    save_account, share_url, sign_in, upload_share,
};
use crate::export::share::{
    DEFAULT_RADIUS_GATEWAY, DEFAULT_SHARE_TITLE, ShareOutcome, ShareRoute, SystemCommandRunner, check_gh_auth,
    create_gist, export_session_for_share, route_needs_radius, share_route, upload_radius_artifact,
    uses_pi_rust_account,
};
use crate::interactive_components::login::{LoginDialog, dialog_interaction};
use crate::interactive_components::share_service::{
    SHARE_SERVICE_TITLE, apply_share_service, parse_share_service, share_service_options, should_ask_share_service,
};
use crate::settings::schema::SessionShareViewer;

/// Removes the temp directory of the share files when dropped.
struct ShareDir(PathBuf);

impl Drop for ShareDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl InteractiveMode {
    /// `/share` to the `sessionShareDestination`: with `auto`, the pi-rust.com account when signed
    /// in (unless `sessionShareViewer` is pi.dev), else Radius when signed in, else a secret GitHub
    /// gist of the HTML export.
    pub(super) fn handle_share_command(&self) {
        // The first /share asks which service to use; Esc cancels and asks again next time.
        if should_ask_share_service(&self.settings()) {
            return self.spawn(move |mode| async move {
                let answer = mode.ask_choice(SHARE_SERVICE_TITLE, share_service_options()).await.ok().flatten();
                match parse_share_service(answer.as_deref()) {
                    Some(service) => {
                        apply_share_service(&mode.settings(), service);
                        mode.handle_share_command();
                    }
                    None => mode.show_status("Share cancelled"),
                }
            });
        }
        let session = self.session();
        let dir = ShareDir(std::env::temp_dir().join(format!("pi-share-{}", crate::session::ids::uuid_v4())));
        let jsonl = dir.0.join("session.jsonl");
        let created = crate::agent_session::output_file::create_private_dir(&dir.0).map_err(|error| {
            anyhow::anyhow!(pi_js::JsError::from_io(&error, "mkdtemp", Some(&dir.0.display().to_string())).message)
        });
        let exported = created.and_then(|()| {
            let file = session.session_file().ok_or_else(|| anyhow::anyhow!("Cannot export in-memory session"))?;
            export_session_for_share(&file, &jsonl, &session.system_prompt(), &session.export_tools())
        });
        if let Err(error) = exported {
            return self.show_error(&format!("Failed to export session: {error}"));
        }
        let destination = self.settings().session_share_destination();
        let viewer = self.settings().session_share_viewer();
        let origin = share_url();
        let account =
            if uses_pi_rust_account(destination, viewer) { load_account_for(&self.agent_dir, &origin) } else { None };
        self.spawn(move |mode| async move {
            let radius =
                if route_needs_radius(destination, account.is_some()) { mode.radius_share_token().await } else { None };
            let route = match share_route(destination, account.is_some(), radius.is_some()) {
                Ok(route) => route,
                Err(error) => return mode.show_error(&error.to_string()),
            };
            let account = match (route, radius) {
                (ShareRoute::Radius, Some(token)) => return mode.share_via_radius(&jsonl, token).await,
                (ShareRoute::PiRustSignIn, _) => match mode.sign_in_to_pi_rust(origin).await {
                    Some(account) => Some(account),
                    None => return,
                },
                _ => account,
            };
            if account.is_none()
                && let Err(error) = check_gh_auth(&SystemCommandRunner).await
            {
                return mode.show_error(&error.to_string());
            }
            let html = dir.0.join("session.html");
            let theme_name = crate::theme::current_theme_name();
            if let Err(error) =
                mode.session().export_to_html_themed(Some(&html.to_string_lossy()), theme_name.as_deref())
            {
                return mode.show_error(&format!("Failed to export session: {error}"));
            }
            match account {
                Some(account) => mode.share_via_pi_rust(&html, account).await,
                None => mode.share_via_gist(&html, viewer).await,
            }
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

    async fn share_via_radius(&self, jsonl: &Path, token: String) {
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

    /// Signs in to pi-rust.com with a device code in a login dialog; Esc cancels. Saves the token
    /// in the agent dir and returns the account.
    async fn sign_in_to_pi_rust(&self, origin: String) -> Option<PiRustAccount> {
        let title = format!("Sign in to {ACCOUNT_LABEL} to share");
        let dialog = LoginDialog::new(&self.tui(), ACCOUNT_LABEL, |_, _| {}, Some(ACCOUNT_LABEL), Some(&title));
        let dialog = Rc::new(RefCell::new(dialog));
        let close = self.mount_view(dialog.clone());
        self.set_blocked_status("share-sign-in", Some((BlockedKind::Auth, format!("Sign in to {ACCOUNT_LABEL}"))));
        let interaction = dialog_interaction(&dialog);
        let cancel = dialog.borrow().cancel_token();
        let client = pi_ai::http::shared_client();
        let result = sign_in(&client, &origin, &interaction, &cancel).await;
        self.set_blocked_status("share-sign-in", None);
        close();
        let account = match result {
            Ok(account) => account,
            Err(PiRustError::Cancelled) => {
                self.show_status("Share cancelled");
                return None;
            }
            Err(error) => {
                self.show_error(&error.to_string());
                return None;
            }
        };
        if let Err(error) = save_account(&self.agent_dir, &account) {
            self.show_warning(&format!("Signed in, but could not save the {ACCOUNT_LABEL} token: {error}"));
        } else {
            self.show_status(&format!("Signed in to {}", account.label()));
        }
        Some(account)
    }

    async fn share_via_pi_rust(&self, html: &Path, account: PiRustAccount) {
        let (loader, cancel) = self.show_share_loader(&format!("Uploading to {ACCOUNT_LABEL}..."));
        let body = match std::fs::read(html) {
            Ok(body) => body,
            Err(error) => {
                loader.borrow_mut().dispose();
                self.selector.restore_editor();
                return self.show_error(&format!("Failed to export session: {error}"));
            }
        };
        let title = self.session().session_name().filter(|name| !name.trim().is_empty());
        let title = title.unwrap_or_else(|| DEFAULT_SHARE_TITLE.to_string());
        let settings = self.settings();
        let options =
            ShareOptions { expiry: settings.session_share_expiry(), visibility: settings.session_share_visibility() };
        let upload = {
            let cancel = cancel.clone();
            async move { upload_share(&pi_ai::http::shared_client(), &account, &title, options, body, &cancel).await }
        };
        let task = tokio::spawn(upload);
        let result = tokio::select! {
            result = task => result,
            () = cancel.cancelled() => return,
        };
        loader.borrow_mut().dispose();
        self.selector.restore_editor();
        match result {
            Ok(Ok(share)) => {
                let link = format!("Share URL: {}", hyperlink(&share.url, &share.url));
                match options.note() {
                    Some(note) => self.show_status(&format!("{link}\n{note}")),
                    None => self.show_status(&link),
                }
            }
            Ok(Err(PiRustError::Cancelled)) => {}
            Ok(Err(PiRustError::Unauthorized)) => {
                // The service revoked the token: forget it so the next /share signs in again.
                let _ = delete_account(&self.agent_dir);
                self.show_error(&PiRustError::Unauthorized.to_string());
            }
            Ok(Err(error)) => self.show_error(&error.to_string()),
            Err(error) => self.show_error(&format!("Failed to share on {ACCOUNT_LABEL}: {error}")),
        }
    }

    async fn share_via_gist(&self, html: &Path, viewer: SessionShareViewer) {
        let (loader, cancel) = self.show_share_loader("Creating gist...");
        let result = tokio::select! {
            result = create_gist(&SystemCommandRunner, html, viewer) => result,
            () = cancel.cancelled() => return,
        };
        loader.borrow_mut().dispose();
        self.selector.restore_editor();
        match result {
            Ok(ShareOutcome::Gist { gist_url, preview_url }) => {
                let preview = hyperlink(&preview_url, &preview_url);
                self.show_status(&format!("Share URL: {preview}\nGist: {}", hyperlink(&gist_url, &gist_url)));
            }
            Ok(ShareOutcome::Radius { url } | ShareOutcome::PiRust { url }) => {
                self.show_status(&format!("Share URL: {}", hyperlink(&url, &url)))
            }
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    /// `/logout` of pi-rust.com: revokes the stored token, then deletes it. A failed revoke still
    /// deletes the local token and reports the failure.
    pub(super) async fn sign_out_of_pi_rust(&self, account: PiRustAccount) {
        let revoke = tokio::spawn(async move { revoke_token(&pi_ai::http::shared_client(), &account).await });
        let revoked =
            revoke.await.map_err(|error| error.to_string()).and_then(|result| result.map_err(|e| e.to_string()));
        if let Err(error) = delete_account(&self.agent_dir) {
            return self.show_error(&format!("Logout failed: {error}"));
        }
        match revoked {
            Ok(()) => self.show_status(&format!("Logged out of {ACCOUNT_LABEL}")),
            Err(error) => self.show_warning(&format!("Removed the {ACCOUNT_LABEL} token from this computer. {error}")),
        }
    }
}
