//! Roots: a root id per `root` call, the code version it runs in, the leases its calls take on objects, and the
//! links it went out on or came over.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};

use super::HostImpl;
use crate::error::{ChordError, ChordResult};
use crate::host2::connection::{ConnId, Msg};
use crate::host2::errors::ChordErrorCode;
use crate::host2::object::{Ctx, CtxFields, HostCore, RootFn, RootOptions, run_root};
use crate::host2::wire::Frame;
use crate::sync::{BoxFuture, lock};

/// A root body with a value (a link's call answers with it).
pub(crate) type ValueFn<T> = Box<dyn FnOnce(Ctx) -> BoxFuture<ChordResult<T>> + Send>;

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
        // A foreign root keeps its leases until the links it came over end it.
        if ended && !host.state().foreign.contains_key(&self.root_id) {
            host.end_root(&self.root_id);
        }
        if ended {
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
        self.join_root(root_id, code_version, run, options, None)
    }

    /// Runs `run` inside root `root_id` of `code_version`. A root counts as running here while any of its
    /// calls runs here; a root that started here ends when its last call here settles. A foreign root (arriving
    /// `from` a link) keeps its leases until every link it came over announced its end or closed.
    pub(crate) fn join_root(
        &self,
        root_id: String,
        code_version: i64,
        run: RootFn,
        options: RootOptions,
        from: Option<ConnId>,
    ) -> BoxFuture<ChordResult<()>> {
        let abort = {
            let mut state = self.state();
            if let Some(from) = from
                && state.code_versions.contains_key(&code_version)
                && !state.local.contains(&root_id)
            {
                state.foreign.entry(root_id.clone()).or_default().insert(from);
            }
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

    /// A new root for a link's call, whose body returns a value.
    pub(crate) fn root_with<T: Send + 'static>(
        &self,
        options: RootOptions,
        run: ValueFn<T>,
    ) -> BoxFuture<ChordResult<T>> {
        run_root(&self.core(), options, run)
    }

    /// [`Self::join_root`] for a link's call, whose body returns a value.
    pub(crate) fn join_with<T: Send + 'static>(
        &self,
        root_id: String,
        code_version: i64,
        options: RootOptions,
        from: Option<ConnId>,
        run: ValueFn<T>,
    ) -> BoxFuture<ChordResult<T>> {
        let slot: Arc<Mutex<Option<T>>> = Arc::default();
        let out = slot.clone();
        let erased: RootFn = Box::new(move |ctx| {
            Box::pin(async move {
                let value = run(ctx).await?;
                *lock(&out) = Some(value);
                Ok(())
            })
        });
        let root = self.join_root(root_id, code_version, erased, options, from);
        Box::pin(async move {
            root.await?;
            let value = lock(&slot).take();
            value.ok_or_else(|| ChordError::error("root settled without a value"))
        })
    }

    /// Calls of `root_id` went out on `conn`, which must hear when the root ends.
    pub(crate) fn root_sent(&self, root_id: &str, conn: ConnId) {
        self.state().root_links.entry(root_id.to_string()).or_default().insert(conn);
    }

    /// A root ended (here, or announced by its origin): its leases end and the links it went out on hear it.
    pub(crate) fn end_root(&self, root_id: &str) {
        let (touched, links) = {
            let mut state = self.state();
            state.local.remove(root_id);
            state.foreign.remove(root_id);
            let links = state.root_links.remove(root_id).unwrap_or_default();
            (state.touches.remove(root_id).unwrap_or_default(), links)
        };
        for object in touched.iter().filter_map(Weak::upgrade) {
            object.root_ended(root_id);
        }
        if let (false, Some(net)) = (links.is_empty(), self.net.get()) {
            for conn in links {
                net.post(Msg::Send { conn, frame: Frame::RootEnd { root_id: root_id.to_string() } });
            }
        }
    }

    /// `from` announced the end of a foreign root (or closed). Honored only if the root came over `from`: a peer
    /// cannot end leases of roots it never carried. The root ends when no link that brought it remains.
    pub(crate) fn foreign_root_ended(&self, root_id: &str, from: ConnId) {
        let end = {
            let mut state = self.state();
            let Some(links) = state.foreign.get_mut(root_id) else { return };
            if !links.remove(&from) || !links.is_empty() {
                return;
            }
            let running = state.code_versions.values().any(|version| version.roots.contains_key(root_id));
            // Still running here (a call in flight): the lease ends when it settles.
            if running {
                state.foreign.remove(root_id);
            }
            !running
        };
        if end {
            self.end_root(root_id);
        }
    }

    /// Retires the older code version once no local roots of it remain (a reload seam: phase 2 has one).
    pub(crate) fn maybe_retire(&self) {}
}
