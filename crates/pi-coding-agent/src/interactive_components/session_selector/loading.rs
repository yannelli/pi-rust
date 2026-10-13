//! Session loading per scope with progress and cancellation, plus delete and rename, which
//! reload the sessions afterwards.

use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{SessionListProgress, SessionScope, SessionSelector, StatusMessage};
use crate::session::SessionInfo;
use crate::session::listing::{DeleteMethod, delete_session_file};

/// A running load (upstream `AbortController`); the id tells stale results apart.
pub(super) struct ActiveLoad {
    id: u64,
    token: CancellationToken,
}

impl ActiveLoad {
    pub(super) fn cancel(&self) {
        self.token.cancel();
    }
}

/// A progress report queued by a loader until the selector can apply it.
pub(super) struct ProgressUpdate {
    scope: SessionScope,
    id: u64,
    loaded: usize,
    total: usize,
    partial: Option<Vec<SessionInfo>>,
}

impl SessionSelector {
    pub(super) fn scope_sessions(&self, scope: SessionScope) -> &Option<Vec<SessionInfo>> {
        match scope {
            SessionScope::Current => &self.current_sessions,
            SessionScope::All => &self.all_sessions,
        }
    }

    fn scope_sessions_mut(&mut self, scope: SessionScope) -> &mut Option<Vec<SessionInfo>> {
        match scope {
            SessionScope::Current => &mut self.current_sessions,
            SessionScope::All => &mut self.all_sessions,
        }
    }

    pub(super) fn active_load(&self, scope: SessionScope) -> Option<&ActiveLoad> {
        match scope {
            SessionScope::Current => self.current_load.as_ref(),
            SessionScope::All => self.all_load.as_ref(),
        }
    }

    fn active_load_slot(&mut self, scope: SessionScope) -> &mut Option<ActiveLoad> {
        match scope {
            SessionScope::Current => &mut self.current_load,
            SessionScope::All => &mut self.all_load,
        }
    }

    fn is_active(&self, scope: SessionScope, id: u64) -> bool {
        self.active_load(scope).is_some_and(|load| load.id == id)
    }

    /// Cancels running loads and forgets what they loaded so far.
    pub(super) fn cancel_loads(&mut self) {
        if let Some(load) = self.current_load.take() {
            load.cancel();
            self.current_sessions = None;
        }
        if let Some(load) = self.all_load.take() {
            load.cancel();
            self.all_sessions = None;
        }
    }

    /// Starts loading `scope` unless a load for it is running. Returns the task that finishes a
    /// load still pending after the first poll.
    pub(super) fn load_scope(&mut self, scope: SessionScope) -> Option<JoinHandle<()>> {
        if self.active_load(scope).is_some() {
            return None;
        }
        let id = self.next_load_id;
        self.next_load_id += 1;
        let token = CancellationToken::new();
        *self.active_load_slot(scope) = Some(ActiveLoad { id, token: token.clone() });
        {
            let mut header = self.header.borrow_mut();
            header.set_scope(scope);
            header.set_loading(true);
        }
        self.render_now();

        let queue = self.progress.clone();
        let progress: SessionListProgress = Rc::new(move |loaded, total, partial| {
            let partial = partial.map(<[SessionInfo]>::to_vec);
            queue.borrow_mut().push(ProgressUpdate { scope, id, loaded, total, partial });
        });
        let loader = match scope {
            SessionScope::Current => &mut self.current_sessions_loader,
            SessionScope::All => &mut self.all_sessions_loader,
        };
        let mut future = loader(progress, token.clone());
        let first = future.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        self.apply_progress();
        if let Poll::Ready(result) = first {
            self.finish_load(scope, id, result);
            return None;
        }
        let this = self.this.clone();
        Some(tokio::task::spawn_local(async move {
            let weak = this.clone();
            let polled = poll_fn(move |cx| {
                let poll = future.as_mut().poll(cx);
                if let Some(selector) = weak.upgrade()
                    && let Ok(mut selector) = selector.try_borrow_mut()
                {
                    selector.apply_progress();
                }
                poll
            });
            let result = tokio::select! {
                biased;
                () = token.cancelled() => return,
                result = polled => result,
            };
            if let Some(selector) = this.upgrade() {
                selector.borrow_mut().finish_load(scope, id, result);
            }
        }))
    }

    /// Applies queued progress of active loads: partial lists and the header counter.
    pub(super) fn apply_progress(&mut self) {
        let updates = std::mem::take(&mut *self.progress.borrow_mut());
        for update in updates {
            if !self.is_active(update.scope, update.id) {
                continue;
            }
            if let Some(partial) = update.partial {
                *self.scope_sessions_mut(update.scope) = Some(partial.clone());
                if update.scope == self.scope {
                    self.session_list.borrow_mut().set_sessions(partial, update.scope == SessionScope::All);
                }
            }
            if update.scope != self.scope {
                continue;
            }
            self.header.borrow_mut().set_progress(update.loaded, update.total);
            self.render_now();
        }
    }

    fn finish_load(&mut self, scope: SessionScope, id: u64, result: anyhow::Result<Vec<SessionInfo>>) {
        self.apply_progress();
        if !self.is_active(scope, id) {
            return;
        }
        *self.active_load_slot(scope) = None;
        let show_cwd = scope == SessionScope::All;
        match result {
            Ok(sessions) => {
                *self.scope_sessions_mut(scope) = Some(sessions.clone());
                if scope != self.scope {
                    return;
                }
                self.header.borrow_mut().set_loading(false);
                self.session_list.borrow_mut().set_sessions(sessions, show_cwd);
            }
            Err(error) => {
                *self.scope_sessions_mut(scope) = None;
                if scope != self.scope {
                    return;
                }
                let message = StatusMessage::error(&format!("Failed to load sessions: {error}"));
                {
                    let mut header = self.header.borrow_mut();
                    header.set_loading(false);
                    header.set_status_message(Some(message), Some(4000));
                }
                self.session_list.borrow_mut().set_sessions(Vec::new(), show_cwd);
            }
        }
        self.render_now();
    }

    fn refresh_sessions_after_mutation(&mut self) -> Option<JoinHandle<()>> {
        self.cancel_loads();
        self.current_sessions = None;
        self.all_sessions = None;
        self.load_scope(self.scope)
    }

    /// Deletes on a blocking thread (`trash`, else unlink), then reloads the sessions.
    pub(super) fn delete_session(&mut self, path: PathBuf) {
        let this = self.this.clone();
        tokio::task::spawn_local(async move {
            let target = path.clone();
            let result = match tokio::task::spawn_blocking(move || delete_session_file(&target)).await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!(error)),
            };
            let refresh = match this.upgrade() {
                Some(selector) => selector.borrow_mut().finish_delete(&path, result),
                None => return,
            };
            if let Some(refresh) = refresh {
                let _ = refresh.await;
            }
            let request_render = this.upgrade().map(|selector| selector.borrow().request_render.clone());
            if let Some(request_render) = request_render {
                request_render();
            }
        });
    }

    fn finish_delete(&mut self, path: &Path, result: anyhow::Result<DeleteMethod>) -> Option<JoinHandle<()>> {
        let method = match result {
            Ok(method) => method,
            Err(error) => {
                let message = StatusMessage::error(&format!("Failed to delete: {error}"));
                self.header.borrow_mut().set_status_message(Some(message), Some(3000));
                return None;
            }
        };
        for sessions in [&mut self.current_sessions, &mut self.all_sessions].into_iter().flatten() {
            sessions.retain(|session| session.path != path);
        }
        let sessions = self.scope_sessions(self.scope).clone().unwrap_or_default();
        self.session_list.borrow_mut().set_sessions(sessions, self.scope == SessionScope::All);
        let message = if method == DeleteMethod::Trash { "Session moved to trash" } else { "Session deleted" };
        self.header.borrow_mut().set_status_message(Some(StatusMessage::info(message)), Some(2000));
        self.refresh_sessions_after_mutation()
    }

    /// Renames the target session, reloads, and leaves rename mode once the reload is done.
    pub(super) fn confirm_rename(&mut self, value: &str) {
        let next = value.trim();
        if next.is_empty() {
            return;
        }
        let (Some(target), Some(rename)) = (self.rename_target_path.clone(), self.rename_session.as_mut()) else {
            return self.exit_rename_mode();
        };
        if let Err(error) = rename(target, next.to_string()) {
            self.exit_rename_mode();
            let message = StatusMessage::error(&format!("Failed to rename: {error}"));
            self.header.borrow_mut().set_status_message(Some(message), Some(3000));
            return self.render_now();
        }
        let Some(refresh) = self.refresh_sessions_after_mutation() else { return self.exit_rename_mode() };
        let this = self.this.clone();
        tokio::task::spawn_local(async move {
            let _ = refresh.await;
            if let Some(selector) = this.upgrade() {
                selector.borrow_mut().exit_rename_mode();
            }
        });
    }
}
