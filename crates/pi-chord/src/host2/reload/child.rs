//! The child side of a cluster reload (upstream `ChildReloads`): prepare and vote, then wait for the server's
//! commit or abort. An abort or a lost parent link can arrive at any point; the operation is unwound once its
//! pending work settles, and the reload slot is freed.

use std::sync::{Arc, Mutex};

use super::{Reload, ReloadOptions, ReloadReport, load};
use crate::error::{ChordError, ChordResult};
use crate::host2::connection::ConnId;
use crate::host2::errors::ChordErrorCode;
use crate::host2::exec::spawn_now;
use crate::host2::host::{HostImpl, PluginRef};
use crate::host2::object::HostCore;
use crate::host2::wire::{Frame, Prepare, Vote, to_wire_error};
use crate::sync::lock;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Loading,
    Preparing,
    Prepared,
}

struct OperationState {
    reload: Option<Arc<Reload>>,
    step: Step,
    cancelled: bool,
}

/// One `prepare` the child received, registered before its first await so an abort or a disconnect is never lost.
pub(crate) struct ChildOperation {
    id: u64,
    state: Mutex<OperationState>,
}

impl ChildOperation {
    /// Moves to `step` unless the operation was cancelled meanwhile.
    fn advance(&self, step: Step, reload: Option<&Arc<Reload>>) -> ChordResult<()> {
        let mut state = lock(&self.state);
        if let Some(reload) = reload {
            state.reload = Some(reload.clone());
        }
        if state.cancelled {
            let message = "reload aborted by the server";
            return Err(ChordError::with_reason(ChordErrorCode::Aborted, "aborted", message));
        }
        state.step = step;
        Ok(())
    }
}

impl HostImpl {
    /// A `prepare`, `commit` or `abort` frame from the parent.
    pub(crate) fn on_reload_frame(&self, conn: ConnId, frame: Frame) {
        let Some(me) = self.me.upgrade() else { return };
        match frame {
            Frame::Prepare(prepare) => me.child_prepare(conn, prepare),
            Frame::Abort { reload } => {
                if let Some(operation) = self.child_operation(reload) {
                    me.cancel_child(&operation);
                }
            }
            Frame::Commit { reload } => {
                let Some(operation) = self.child_operation(reload) else { return };
                let reload = {
                    let state = lock(&operation.state);
                    if state.step != Step::Prepared {
                        return;
                    }
                    state.reload.clone()
                };
                self.clear_child_operation(&operation);
                if let Some(Err(error)) = reload.map(|reload| reload.commit(None)) {
                    self.report(error);
                }
                self.reload_ended();
            }
            _ => {}
        }
    }

    /// The parent link died: nobody will commit, so roll back (now, or once pending work settles).
    pub(crate) fn parent_lost(&self) {
        let operation = lock(&self.child_reload).clone();
        if let (Some(operation), Some(me)) = (operation, self.me.upgrade()) {
            me.cancel_child(&operation);
        }
    }

    fn child_operation(&self, id: u64) -> Option<Arc<ChildOperation>> {
        lock(&self.child_reload).clone().filter(|operation| operation.id == id)
    }

    fn clear_child_operation(&self, operation: &Arc<ChildOperation>) {
        let mut slot = lock(&self.child_reload);
        if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, operation)) {
            *slot = None;
        }
    }

    fn child_prepare(self: &Arc<Self>, conn: ConnId, prepare: Prepare) {
        let operation = {
            let mut slot = lock(&self.child_reload);
            if slot.is_some() {
                drop(slot);
                let message = "a reload is already running";
                let error = ChordError::with_reason(ChordErrorCode::Unavailable, "reload_running", message);
                let vote = Vote { reload: prepare.reload, error: Some(to_wire_error(&error)), report: None };
                self.net().vote(conn, vote);
                return;
            }
            let state = OperationState { reload: None, step: Step::Loading, cancelled: false };
            let operation = Arc::new(ChildOperation { id: prepare.reload, state: Mutex::new(state) });
            *slot = Some(operation.clone());
            operation
        };
        let host = self.clone();
        spawn_now(self.handle(), async move { host.run_child(operation, conn, prepare).await });
    }

    async fn run_child(self: Arc<Self>, operation: Arc<ChildOperation>, conn: ConnId, prepare: Prepare) {
        let id = prepare.reload;
        let vote = match self.prepare_child(&operation, prepare).await {
            Ok(summary) => Vote { reload: id, error: None, report: Some(summary.to_wire()) },
            Err(error) => {
                let reload = lock(&operation.state).reload.clone();
                if let Some(reload) = reload {
                    reload.rollback().await;
                    self.reload_ended();
                }
                self.clear_child_operation(&operation);
                if lock(&operation.state).cancelled {
                    return;
                }
                Vote { reload: id, error: Some(to_wire_error(&error)), report: None }
            }
        };
        self.net().vote(conn, vote);
    }

    async fn prepare_child(
        self: &Arc<Self>,
        operation: &ChildOperation,
        prepare: Prepare,
    ) -> ChordResult<ReloadReport> {
        self.begin_reload()?;
        let plugins = prepare.plugins.into_iter().map(PluginRef::Module).collect();
        let options = ReloadOptions::from_wire(&prepare.options);
        let reload = load(self, plugins, options, Some(prepare.code_version as i64)).await?;
        operation.advance(Step::Preparing, Some(&reload))?;
        reload.prepare().await?;
        operation.advance(Step::Prepared, None)?;
        Ok(reload.summary())
    }

    fn cancel_child(self: &Arc<Self>, operation: &Arc<ChildOperation>) {
        let (prepared, reload) = {
            let mut state = lock(&operation.state);
            state.cancelled = true;
            (state.step == Step::Prepared, state.reload.clone())
        };
        let Some(reload) = reload else { return };
        if !prepared {
            // Loading or preparing: prepare fails now; `run_child` unwinds when it settles.
            spawn_now(self.handle(), async move { reload.rollback().await });
            return;
        }
        self.clear_child_operation(operation);
        let host = self.clone();
        spawn_now(self.handle(), async move {
            reload.rollback().await;
            host.reload_ended();
        });
    }
}
