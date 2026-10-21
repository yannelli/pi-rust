//! Roots: a root id per `root` call, the code version it runs in, and the leases its calls take on objects.

use std::sync::Weak;
use std::sync::atomic::Ordering;

use super::HostImpl;
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::ChordErrorCode;
use crate::host2::object::{Ctx, CtxFields, HostCore, RootFn, RootOptions};
use crate::sync::BoxFuture;

/// Ends the root's run here when dropped: the context closes and the root's count in its code version drops.
struct RootRun {
    host: Weak<HostImpl>,
    root_id: String,
    code_version: i64,
    ctx: Ctx,
}

impl Drop for RootRun {
    fn drop(&mut self) {
        self.ctx.close();
        let Some(host) = self.host.upgrade() else { return };
        let ended = {
            let mut state = host.state();
            match state.code_versions.get_mut(&self.code_version) {
                Some(version) => {
                    let count = version.roots.get(&self.root_id).copied().unwrap_or(1).saturating_sub(1);
                    if count > 0 {
                        version.roots.insert(self.root_id.clone(), count);
                    } else {
                        version.roots.remove(&self.root_id);
                    }
                    count == 0
                }
                None => true,
            }
        };
        if ended {
            host.end_root(&self.root_id);
            host.maybe_retire();
        }
    }
}

impl HostImpl {
    pub(super) fn start_root(&self, run: RootFn, options: RootOptions) -> BoxFuture<ChordResult<()>> {
        let seq = self.root_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let root_id = format!("{}#{}:{seq}", self.name(), self.incarnation);
        let code_version = {
            let mut state = self.state();
            state.local.insert(root_id.clone());
            options.code_version.unwrap_or(state.current)
        };
        self.join_root(root_id, code_version, run, options)
    }

    /// Runs `run` inside root `root_id` of `code_version`. A root counts as running here while any of its
    /// calls runs here; a root that started here ends when its last call here settles.
    pub(crate) fn join_root(
        &self,
        root_id: String,
        code_version: i64,
        run: RootFn,
        options: RootOptions,
    ) -> BoxFuture<ChordResult<()>> {
        let abort = {
            let mut state = self.state();
            match state.code_versions.get_mut(&code_version) {
                Some(version) => {
                    *version.roots.entry(root_id.clone()).or_insert(0) += 1;
                    version.abort.signal()
                }
                None => {
                    state.local.remove(&root_id);
                    let message = format!("code version {code_version} is not retained by {}", self.name());
                    let error = ChordError::code_only(ChordErrorCode::Superseded, message);
                    return Box::pin(std::future::ready(Err(error)));
                }
            }
        };
        // Forced retirement aborts what still runs on a code version before closing what only it used.
        let signals = match options.signal {
            Some(signal) => vec![signal, abort],
            None => vec![abort],
        };
        let fields = CtxFields {
            signals,
            root_id: root_id.clone(),
            code_version,
            principal: options.principal.unwrap_or_else(|| self.principal().clone()),
            holder: false,
        };
        let ctx = Ctx::new(self.core(), fields);
        let guard = RootRun { host: self.me.clone(), root_id, code_version, ctx: ctx.clone() };
        let running = run(ctx);
        Box::pin(async move {
            let _guard = guard;
            running.await
        })
    }

    /// A root ended: its leases end.
    pub(crate) fn end_root(&self, root_id: &str) {
        let touched = {
            let mut state = self.state();
            state.local.remove(root_id);
            state.touches.remove(root_id).unwrap_or_default()
        };
        for object in touched.iter().filter_map(Weak::upgrade) {
            object.root_ended(root_id);
        }
    }

    /// Retires the older code version once no local roots of it remain (a reload seam: phase 2 has one).
    pub(crate) fn maybe_retire(&self) {}
}
