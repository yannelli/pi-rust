//! Object lifetime: references, idling (with `linger`) and disposal.

use std::sync::Arc;

use futures::FutureExt;
use futures::future::join_all;
use tokio::runtime::Handle;

use super::{ObjectImpl, ObjectStatus, Ref, RefImpl, RefOwner};
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::Contract;
use crate::host2::errors::{ChordErrorCode, run_all};
use crate::host2::exec::spawn_now;
use crate::sync::{BoxFuture, SharedTask, lock};

enum Idle {
    Dispose,
    Linger(std::time::Duration),
}

impl ObjectImpl {
    pub(crate) fn refs(&self) -> Vec<Arc<RefImpl>> {
        self.state().refs.values().cloned().collect()
    }

    /// Disposing, or about to: its disposal started and has not finished.
    pub(crate) fn disposing(&self) -> bool {
        let state = self.state();
        state.status == ObjectStatus::Disposing || (state.disposal.is_some() && state.status != ObjectStatus::Disposed)
    }

    /// A new reference, unless the object is past activation or live (the caller looks it up again).
    pub(crate) fn new_ref(self: &Arc<Self>, contract: Contract, owner: RefOwner) -> Option<Arc<RefImpl>> {
        let mut state = self.state();
        if !matches!(state.status, ObjectStatus::Activating | ObjectStatus::Live) || state.disposal.is_some() {
            return None;
        }
        let reference = RefImpl::new(self.clone(), contract, owner, state.status == ObjectStatus::Live);
        state.refs.insert(reference.id(), reference.clone());
        if let Some((_, Some(timer))) = state.linger.take() {
            timer.abort();
        }
        Some(reference)
    }

    pub(crate) fn release_ref(&self, reference: &RefImpl) {
        let removed = self.state().refs.shift_remove(&reference.id()).is_some();
        if removed && let Some(me) = self.me() {
            me.maybe_idle();
        }
    }

    /// No references and no invocations: dispose (after `linger` for a live object).
    pub(crate) fn maybe_idle(self: &Arc<Self>) {
        let idle = {
            let state = self.state();
            if !state.refs.is_empty() || state.invocations > 0 || state.linger.is_some() {
                return;
            }
            let linger = state.service.as_ref().and_then(|service| service.options.linger);
            match (state.status, linger) {
                (ObjectStatus::Activating, _) => Idle::Dispose,
                (ObjectStatus::Live, Some(linger)) if !linger.is_zero() => Idle::Linger(linger),
                (ObjectStatus::Live, _) => Idle::Dispose,
                _ => return,
            }
        };
        let linger = match idle {
            Idle::Dispose => return self.background(self.dispose(false, None)),
            Idle::Linger(linger) => linger,
        };
        let mut state = self.state();
        if state.linger.is_some() {
            return;
        }
        state.linger_seq += 1;
        let seq = state.linger_seq;
        let object = Arc::downgrade(self);
        let timer = async move {
            tokio::time::sleep(linger).await;
            let Some(object) = object.upgrade() else { return };
            let idle = {
                let mut state = object.state();
                if state.linger.as_ref().map(|(current, _)| *current) != Some(seq) {
                    return;
                }
                state.linger = None;
                state.refs.is_empty() && state.invocations == 0 && state.status == ObjectStatus::Live
            };
            if idle {
                object.background(object.dispose(false, None));
            }
        };
        let handle = self.host.handle().cloned().or_else(|| Handle::try_current().ok());
        let timer = match handle {
            Some(handle) => Some(handle.spawn(timer).abort_handle()),
            None => {
                std::thread::spawn(move || futures::executor::block_on(timer));
                None
            }
        };
        state.linger = Some((seq, timer));
    }

    fn background(&self, disposal: SharedTask) {
        let host = self.host.clone();
        spawn_now(self.host.handle(), async move {
            if let Err(error) = disposal.await {
                host.report(error);
            }
        });
    }

    /// Disposes the object (once). Forced: references become gone at once and running invocations are
    /// cancelled. Either way, resources close only after every invocation settled.
    pub(crate) fn dispose(self: &Arc<Self>, forced: bool, reason: Option<ChordError>) -> SharedTask {
        let task = {
            let mut state = self.state();
            if let Some(disposal) = &state.disposal {
                return disposal.clone();
            }
            let object = self.clone();
            let task: BoxFuture<ChordResult<()>> = Box::pin(async move { object.dispose_inner(forced, reason).await });
            let task = task.shared();
            state.disposal = Some(task.clone());
            task
        };
        let driver = task.clone();
        spawn_now(self.host.handle(), async move {
            let _ = driver.await;
        });
        task
    }

    async fn dispose_inner(self: &Arc<Self>, forced: bool, reason: Option<ChordError>) -> ChordResult<()> {
        let was_activating = {
            let mut state = self.state();
            let was_activating = state.status == ObjectStatus::Activating;
            state.status = ObjectStatus::Disposing;
            if let Some((_, Some(timer))) = state.linger.take() {
                timer.abort();
            }
            was_activating
        };
        self.host.forget(self);
        let error = reason
            .unwrap_or_else(|| ChordError::code_only(ChordErrorCode::Gone, format!("{} was disposed", self.address())));
        if forced {
            self.refs().iter().for_each(|reference| reference.gone(&error));
            self.call_abort.abort(Some(error.clone()));
        }
        self.activation_abort.abort(Some(error.clone()));
        if was_activating {
            let _ = self.ready().await;
        }
        self.refs().iter().for_each(|reference| reference.gone(&error));
        let decls = self.all_decls();
        self.stop_tasks();
        self.stop_listeners();
        self.wait_invocations().await;
        let registered = {
            let mut state = self.state();
            let registered =
                decls.iter().any(|decls| !decls.data().registrations.is_empty()) && !state.deferred_runtime;
            state.implementations.clear();
            state.decls = Arc::default();
            registered
        };
        if registered {
            self.host.registries_changed(None);
        }
        let mut refs: Vec<Ref> = Vec::new();
        for decls in &decls {
            let (derives, held): (Vec<_>, Vec<_>) = {
                let data = decls.data();
                (data.derives.values().cloned().collect(), data.refs.values().cloned().collect())
            };
            derives.iter().for_each(|derived| derived.detach());
            for reference in held {
                if !refs.iter().any(|existing| existing.ptr_eq(&reference)) {
                    refs.push(reference);
                }
            }
        }
        let stopped: Vec<SharedTask> = self.state().stopped.values().cloned().collect();
        let mut steps: Vec<BoxFuture<ChordResult<()>>> = vec![Box::pin(async move {
            join_all(stopped).await;
            Ok(())
        })];
        let order = lock(&self.open_order).clone();
        for resource in order.into_iter().rev() {
            let object = self.clone();
            steps.push(Box::pin(async move { resource.close(&object.open_order).await }));
        }
        for reference in refs {
            steps.push(Box::pin(async move {
                reference.release();
                Ok(())
            }));
        }
        let result = run_all(steps, &format!("{}: disposal failed", self.address())).await;
        {
            let mut state = self.state();
            state.status = ObjectStatus::Disposed;
            // Every reference is gone: dropping them breaks the cycle between them and the object.
            state.refs.clear();
        }
        self.host.disposed(self);
        result
    }
}
