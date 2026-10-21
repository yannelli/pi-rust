//! Port of upstream `reload.ts` and `cluster.ts`: one reload on one host (prepare, gate, then commit or rollback),
//! cluster reloads the server coordinates, and the child side of them. Everything that can fail runs in prepare;
//! the commit installs the new code version in one synchronous step.

mod child;
mod cluster;
mod commit;
mod gate;
mod prepare;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use indexmap::{IndexMap, IndexSet};

pub(crate) use child::ChildOperation;

use super::cell::Cell;
use super::errors::{ChordErrorCode, ErrorOptions, as_chord_error};
use super::exec::spawn_now;
use super::host::{ErasedTable, Facet, Host, HostImpl, PluginRef};
use super::object::{HostCore, ObjectImpl, ObjectStatus, ResourceImpl, Run, Service, StagedListeners};
use super::wire::{PeerKind, WireReloadOptions, WireReloadReport};
use crate::context::AbortController;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{BoxFuture, SharedTask, lock};

/// `reload` options.
#[derive(Clone, Debug, Default)]
pub struct ReloadOptions {
    /// Facet names to remove.
    pub remove: Vec<String>,
    /// How long the gate waits for grandfathered roots (default 5 s). Missed: the reload aborts.
    pub gate_deadline: Option<Duration>,
    /// How long to wait for the older code version to retire first (default 5 s). Missed: it is retired anyway.
    pub retire_deadline: Option<Duration>,
    /// Server: facet modules for child hosts by host-name prefix (longest wins). Every connected host takes part.
    pub remote: Option<IndexMap<String, Vec<String>>>,
    /// Server: facet names to remove on child hosts, by the same host-name prefix rules.
    pub remote_remove: Option<IndexMap<String, Vec<String>>>,
    /// Server: how long to wait for every child's vote (default: gate and retire deadlines plus 5 s).
    pub vote_deadline: Option<Duration>,
}

const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);

impl ReloadOptions {
    fn gate_deadline(&self) -> Duration {
        self.gate_deadline.unwrap_or(DEFAULT_DEADLINE)
    }

    fn retire_deadline(&self) -> Duration {
        self.retire_deadline.unwrap_or(DEFAULT_DEADLINE)
    }

    /// The options a parent sent with its `prepare`.
    fn from_wire(options: &WireReloadOptions) -> Self {
        let millis = |ms: f64| Duration::from_secs_f64(ms.max(0.0) / 1000.0);
        Self {
            remove: options.remove.clone().unwrap_or_default(),
            gate_deadline: options.gate_deadline.map(millis),
            retire_deadline: options.retire_deadline.map(millis),
            ..Self::default()
        }
    }
}

/// What a reload did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReloadReport {
    pub code_version: i64,
    pub previous: i64,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    /// Addresses of objects whose new factory was installed.
    pub prepared: Vec<String>,
    /// Addresses of objects that went through the gate.
    pub gated: Vec<String>,
    /// Cluster reloads: what each child host prepared, by host name (its vote). Empty for local reloads.
    pub children: IndexMap<String, ReloadReport>,
}

impl ReloadReport {
    pub(crate) fn to_wire(&self) -> WireReloadReport {
        WireReloadReport {
            code_version: self.code_version,
            previous: self.previous,
            changed: self.changed.clone(),
            removed: self.removed.clone(),
            prepared: self.prepared.clone(),
            gated: self.gated.clone(),
        }
    }

    pub(crate) fn from_wire(report: WireReloadReport) -> Self {
        Self {
            code_version: report.code_version,
            previous: report.previous,
            changed: report.changed,
            removed: report.removed,
            prepared: report.prepared,
            gated: report.gated,
            children: IndexMap::new(),
        }
    }
}

/// A reload error keeps the underlying code (`contract_mismatch`, `invalid`, ...) and says it aborted the reload.
pub(crate) fn reload_error(error: ChordError) -> ChordError {
    let cause = as_chord_error(error, "reload");
    let message = cause.to_string();
    let bare = match message.find(':') {
        Some(index) if message[index..].starts_with(": ") => &message[index + 2..],
        _ => message.as_str(),
    };
    if bare.starts_with("reload aborted") {
        return cause;
    }
    let code = cause.code().unwrap_or(ChordErrorCode::Unavailable);
    let options = ErrorOptions {
        reason: cause.reason().map(str::to_string),
        details: cause.details().cloned(),
        cause: Some(cause.clone()),
    };
    ChordError::coded(code, format!("reload aborted: {message}"), options)
}

/// One object's prepared next run.
pub(super) struct Candidate {
    pub object: Arc<ObjectImpl>,
    pub service: Arc<Service>,
    pub run: Arc<Run>,
    /// Why the object needs the gate (empty: compatible).
    pub reasons: Vec<String>,
    /// New exclusive resources and their new dependents, opened at the gate.
    pub at_gate: Vec<Arc<ResourceImpl>>,
    /// Old exclusive resources closed at the gate.
    pub replaced: Vec<Arc<ResourceImpl>>,
    pub migrated: Vec<(Arc<Cell>, Json)>,
    /// Values of the run's derivations over the migrated values, validated, by source id.
    pub derived: HashMap<usize, Json>,
    /// Listener subscriptions made at the end of prepare, activated by the commit.
    pub staged: Option<StagedListeners>,
}

/// Work that may start new-code-version roots, run after the install (and after the coordinator's commit frames).
type Start = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub(super) struct Inner {
    pub candidates: IndexMap<usize, Candidate>,
    /// Replaced exclusive resources, closed at the gate: a rollback reopens them.
    pub closed: Vec<(Arc<ObjectImpl>, Arc<ResourceImpl>)>,
    pub services: IndexMap<String, Arc<Service>>,
    pub changed: IndexSet<String>,
    pub removed: IndexSet<String>,
    pub tables: HashMap<String, ErasedTable>,
    /// Keys of the candidates that went through the gate.
    pub gated: Vec<usize>,
    pub rolled_back: bool,
    /// Objects of types this reload adds, with their staged listeners: their runtime starts at commit.
    pub deferred: Vec<(Arc<ObjectImpl>, StagedListeners)>,
    /// This reload's run of each facet it loads: installed at commit, aborted at rollback.
    pub facet_runs: IndexMap<String, AbortController>,
    pub starts: Vec<Start>,
}

/// One reload on one host: `prepare` (load, prepare, gate), then `commit` or `rollback`.
pub(crate) struct Reload {
    host: Arc<HostImpl>,
    facets: Vec<Facet>,
    options: ReloadOptions,
    /// The code version id to commit (issued by the coordinating server); a local reload takes the next one.
    code_version: Option<i64>,
    abort: AbortController,
    inner: Mutex<Inner>,
    rollback: Mutex<Option<SharedTask>>,
}

/// The key of an object in a reload's maps.
pub(super) fn key(object: &Arc<ObjectImpl>) -> usize {
    Arc::as_ptr(object) as usize
}

impl Reload {
    fn new(host: Arc<HostImpl>, facets: Vec<Facet>, options: ReloadOptions, code_version: Option<i64>) -> Arc<Self> {
        let (inner, rollback) = (Mutex::default(), Mutex::default());
        Arc::new(Self { host, facets, options, code_version, abort: AbortController::new(), inner, rollback })
    }

    pub(super) fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    /// After every await of prepare: a rollback meanwhile (an abort from the coordinator) ends it.
    pub(super) fn alive(&self) -> ChordResult<()> {
        if self.inner().rolled_back {
            let message = "reload aborted while preparing";
            return Err(ChordError::with_reason(ChordErrorCode::Aborted, "aborted", message));
        }
        Ok(())
    }

    /// What a successful prepare will commit (a child sends it with its vote).
    pub(crate) fn summary(&self) -> ReloadReport {
        let (pending, current) = {
            let state = self.host.state();
            (state.pending.as_ref().map(|pending| pending.id), state.current)
        };
        let inner = self.inner();
        let live = inner.candidates.values().filter(|candidate| candidate.object.status() == ObjectStatus::Live);
        let gated = inner.gated.iter().filter_map(|key| inner.candidates.get(key));
        ReloadReport {
            code_version: pending.unwrap_or(current),
            previous: current,
            changed: inner.changed.iter().cloned().collect(),
            removed: inner.removed.iter().cloned().collect(),
            prepared: live.map(|candidate| candidate.object.address().to_string()).collect(),
            gated: gated.map(|candidate| candidate.object.address().to_string()).collect(),
            children: IndexMap::new(),
        }
    }
}

/// Loads the plugin modules of a reload that took the host's reload slot. On failure the slot is freed; on
/// success the caller frees it.
async fn load(
    host: &Arc<HostImpl>,
    plugins: Vec<PluginRef>,
    options: ReloadOptions,
    code_version: Option<i64>,
) -> ChordResult<Arc<Reload>> {
    match host.resolve(plugins).await {
        Ok(facets) => Ok(Reload::new(host.clone(), facets, options, code_version)),
        Err(error) => {
            host.reload_ended();
            Err(reload_error(error))
        }
    }
}

/// `Host::reload`: a cluster reload when child hosts are connected (or remote modules are given), else local.
async fn run(host: Arc<HostImpl>, plugins: Vec<PluginRef>, options: ReloadOptions) -> ChordResult<ReloadReport> {
    // Client-local facet code versions never cross the parent link; only child hosts need the server.
    if host.kind() != PeerKind::Client && host.has_parent() {
        let message = "reloads are coordinated by the server";
        return Err(ChordError::with_reason(ChordErrorCode::Unavailable, "not_coordinator", message));
    }
    host.begin_reload()?;
    let (children, synced) = match host.net_if_started() {
        Some(net) => net.child_hosts().await,
        None => (false, Vec::new()),
    };
    if children || options.remote.is_some() || options.remote_remove.is_some() {
        return cluster::run_cluster_reload(host, plugins, options, synced).await;
    }
    let reload = load(&host, plugins, options, None).await?;
    let result = match reload.prepare().await {
        Ok(()) => reload.commit(None),
        Err(error) => Err(error),
    };
    host.reload_ended();
    result
}

impl Host {
    /// Reloads the given facets (modules are loaded again); facets not listed keep running unchanged.
    pub fn reload(&self, plugins: Vec<PluginRef>, options: ReloadOptions) -> BoxFuture<ChordResult<ReloadReport>> {
        let host = self.inner().clone();
        let (done, result) = tokio::sync::oneshot::channel();
        let handle = host.handle().cloned();
        spawn_now(handle.as_ref(), async move {
            let _ = done.send(run(host, plugins, options).await);
        });
        Box::pin(async move { result.await.unwrap_or_else(|_| Err(ChordError::error("reload dropped"))) })
    }
}
