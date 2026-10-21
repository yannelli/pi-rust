//! Code version bookkeeping: ids, the two-version rule (a host keeps the current and one older code version),
//! retirement, the end of a reload, and the modules cluster reloads gave child hosts.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use indexmap::{IndexMap, IndexSet};
use tokio::sync::oneshot;

use super::{Host, HostImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::{ChordErrorCode, run_all};
use crate::host2::exec::spawn_now;
use crate::host2::object::{ObjectImpl, ResourceImpl};
use crate::host2::wire::PeerKind;
use crate::sync::{BoxFuture, lock};

/// Identity of an allocation, for sets of shared objects.
pub(crate) fn id<T: ?Sized>(value: &Arc<T>) -> usize {
    Arc::as_ptr(value) as *const () as usize
}

impl HostImpl {
    /// The older code version kept beside the current one.
    pub(crate) fn previous_id(&self) -> Option<i64> {
        self.state().previous
    }

    /// Roots of `code_version` running here.
    pub(crate) fn roots_of(&self, code_version: i64) -> usize {
        self.state().code_versions.get(&code_version).map_or(0, |version| version.roots.len())
    }

    pub(crate) fn has_parent(&self) -> bool {
        self.state().parent
    }

    pub(crate) fn set_parent(&self, parent: bool) {
        self.state().parent = parent;
    }

    /// The id the next reload commits: `given` by the coordinating server, or the next one here.
    pub(crate) fn next_code_version_id(&self, given: Option<i64>) -> ChordResult<i64> {
        let mut state = self.state();
        if let Some(given) = given
            && given <= state.current
        {
            return Err(ChordError::error(format!("code version {given} is not newer than {}", state.current)));
        }
        state.code_version_seq = given.unwrap_or(state.code_version_seq + 1);
        Ok(state.code_version_seq)
    }

    /// The code version id the next reload would commit.
    pub(crate) fn peek_next_code_version(&self) -> i64 {
        self.state().code_version_seq + 1
    }

    /// Retires the older code version once no local roots of it remain. A child also waits for its parent to
    /// retire it: roots span hosts, and an old root on the server may still call in.
    pub(crate) fn maybe_retire(&self) {
        if self.retirable() {
            self.retire();
        }
    }

    /// No roots of the older code version are left here and, on a child host, the parent retired it.
    pub(crate) fn retirable(&self) -> bool {
        let state = self.state();
        let Some(older) = state.previous else { return false };
        if state.code_versions.get(&older).is_some_and(|version| !version.roots.is_empty()) {
            return false;
        }
        self.kind == PeerKind::Client || !state.parent || older <= state.retire_allowed
    }

    /// The parent retired `code_version`: this host may retire it too.
    pub(crate) fn allow_retire(&self, code_version: u64) {
        {
            let mut state = self.state();
            state.retire_allowed = state.retire_allowed.max(code_version as i64);
        }
        self.maybe_retire();
        self.retirement_changed();
    }

    pub(crate) fn retirement_changed(&self) {
        self.retirement.notify_waiters();
    }

    /// True once `ready` holds (checked whenever retirement-relevant state changes), false after `limit`.
    pub(crate) async fn retired_until(&self, ready: impl Fn() -> bool, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let notified = self.retirement.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ready() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return false;
            }
        }
    }

    /// Every code version below the oldest one this host keeps is retired (ids only grow).
    pub(crate) fn retired_floor(&self) -> i64 {
        let state = self.state();
        state.previous.unwrap_or(state.current) - 1
    }

    /// Drops the older code version: its implementations and the declarations only it used.
    pub(crate) fn retire(&self) {
        let (older, objects) = {
            let mut state = self.state();
            let Some(older) = state.previous.take() else { return };
            state.code_versions.shift_remove(&older);
            (older, state.objects.values().cloned().collect::<Vec<_>>())
        };
        for object in objects {
            self.retire_implementation(&object, older);
        }
        if let Some(net) = self.net.get() {
            net.retired(older as u64);
        }
        self.retirement_changed();
    }

    /// Drops an object's implementation for `code_version` and what only that implementation declared, in
    /// disposal order: resources close (reverse open order) before its references are released.
    pub(crate) fn retire_implementation(&self, object: &Arc<ObjectImpl>, code_version: i64) {
        let (installed, decls) = {
            let mut state = object.state();
            (state.implementations.shift_remove(&code_version), state.decls.clone())
        };
        let Some(installed) = installed else { return };
        if Arc::ptr_eq(&installed.decls, &decls) {
            return;
        }
        let (mut refs, mut resources, mut derives) = (HashSet::new(), HashSet::new(), HashSet::new());
        for kept in object.all_decls() {
            let data = kept.data();
            refs.extend(data.refs.values().map(|reference| id(reference.core())));
            resources.extend(data.resources.values().map(id));
            derives.extend(data.derives.values().map(id));
        }
        let (dropped, mut closing, releasing) = {
            let data = installed.decls.data();
            let dropped = data.derives.values().filter(|derived| !derives.contains(&id(derived))).cloned();
            let closing = data.resources.values().filter(|resource| !resources.contains(&id(resource))).cloned();
            let releasing = data.refs.values().filter(|reference| !refs.contains(&id(reference.core()))).cloned();
            (dropped.collect::<Vec<_>>(), closing.collect::<Vec<_>>(), releasing.collect::<Vec<_>>())
        };
        dropped.iter().for_each(|derived| derived.detach());
        let order = lock(&object.open_order).clone();
        let position = |resource: &Arc<ResourceImpl>| order.iter().position(|open| Arc::ptr_eq(open, resource));
        closing.sort_by_key(|resource| std::cmp::Reverse(position(resource).map_or(-1, |index| index as i64)));
        object.refresh_schemas();
        let mut steps: Vec<BoxFuture<ChordResult<()>>> = Vec::new();
        for resource in closing {
            let object = object.clone();
            steps.push(Box::pin(async move { resource.close(&object.open_order).await }));
        }
        for reference in releasing {
            steps.push(Box::pin(async move {
                reference.release();
                Ok(())
            }));
        }
        let message = format!("{}: retiring code version {code_version} failed", object.address());
        let host = object.host().clone();
        spawn_now(self.handle.as_ref(), async move {
            if let Err(error) = run_all(steps, &message).await {
                host.report(error);
            }
        });
    }

    /// Takes the host's reload slot: a second reload while one runs is refused.
    pub(crate) fn begin_reload(&self) -> ChordResult<()> {
        let mut state = self.state();
        if state.reloading {
            let message = "a reload is already running";
            return Err(ChordError::with_reason(ChordErrorCode::Unavailable, "reload_running", message));
        }
        state.reloading = true;
        Ok(())
    }

    /// The host's reload slot is free again: child hosts whose hello came meanwhile are synced or reconciled now.
    pub(crate) fn reload_ended(&self) {
        let waiters = {
            let mut state = self.state();
            state.reloading = false;
            std::mem::take(&mut state.reload_waiters)
        };
        if let Some(net) = self.net.get() {
            net.reload_ended();
        }
        for waiter in waiters {
            let _ = waiter.send(());
        }
        self.maybe_retire();
    }

    /// Settles once no reload runs on this host.
    pub(crate) fn when_reload_ended(&self) -> BoxFuture<()> {
        let mut state = self.state();
        if !state.reloading {
            return Box::pin(std::future::ready(()));
        }
        let (done, wait) = oneshot::channel();
        state.reload_waiters.push(done);
        Box::pin(async move {
            let _ = wait.await;
        })
    }

    /// Remembers what a committed cluster reload loaded and removed on child hosts, by host-name prefix.
    pub(crate) fn remember_remote_modules(
        &self,
        remote: Option<&IndexMap<String, Vec<String>>>,
        remove: Option<&IndexMap<String, Vec<String>>>,
    ) {
        let empty = IndexMap::new();
        let (remote, remove) = (remote.unwrap_or(&empty), remove.unwrap_or(&empty));
        let mut state = self.state();
        let prefixes: IndexSet<String> = remote.keys().chain(remove.keys()).cloned().collect();
        for prefix in prefixes {
            let modules = remote.get(&prefix).cloned().unwrap_or_default();
            let removed = remove.get(&prefix).cloned().unwrap_or_default();
            let known = state.remote_modules.get(&prefix).cloned().unwrap_or_default();
            let kept = merged(known, &modules).into_iter().filter(|module| !removed.contains(module)).collect();
            let gone = state.remote_removed.get(&prefix).cloned().unwrap_or_default();
            let gone = merged(gone, &removed).into_iter().filter(|module| !modules.contains(module)).collect();
            state.remote_modules.insert(prefix.clone(), kept);
            state.remote_removed.insert(prefix, gone);
        }
    }

    /// The modules (and removals) committed cluster reloads gave hosts named `host_name`.
    pub(crate) fn remote_modules_for(&self, host_name: &str) -> (Vec<String>, Vec<String>) {
        let state = self.state();
        (modules_for(Some(&state.remote_modules), host_name), modules_for(Some(&state.remote_removed), host_name))
    }
}

/// `list` then `more`, each module once, in first-seen order.
fn merged(list: Vec<String>, more: &[String]) -> Vec<String> {
    let set: IndexSet<String> = list.into_iter().chain(more.iter().cloned()).collect();
    set.into_iter().collect()
}

/// The entry with the longest prefix of `host_name` (upstream `modulesFor`).
pub(crate) fn modules_for(by_prefix: Option<&IndexMap<String, Vec<String>>>, host_name: &str) -> Vec<String> {
    let matching = by_prefix.into_iter().flatten().filter(|(prefix, _)| host_name.starts_with(prefix.as_str()));
    matching.max_by_key(|(prefix, _)| prefix.len()).map(|(_, modules)| modules.clone()).unwrap_or_default()
}

impl Host {
    /// Whether a reload runs on this host now.
    pub fn reloading(&self) -> bool {
        self.0.state().reloading
    }

    /// The older code version kept beside the current one until it retires.
    pub fn previous_code_version(&self) -> Option<i64> {
        self.0.previous_id()
    }
}
