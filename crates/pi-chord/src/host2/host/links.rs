//! Links and remote references on a host: accepting, connecting and routing links, the shared bindings of remote
//! references (with lingering), and the bookkeeping connections ask of their host.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot;

use super::{Host, HostImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::connection::{
    AcceptOptions, ConnId, ConnectOptions, ConnectionInfo, Msg, NetHandle, Route, spawn_net,
};
use crate::host2::contract::Contract;
use crate::host2::exec::ErrorHandler;
use crate::host2::link::Link;
use crate::host2::object::{HostCore, Ref};
use crate::host2::remote::{RemoteBinding, RemoteRef};
use crate::host2::types::GetOptions;
use crate::host2::wire::PeerKind;
use crate::json::Json;
use crate::sync::BoxFuture;

/// Remote bindings kept after their last reference is released: a binding of a listed type stays held (and
/// hydrated) for its duration, so a reference taken again meanwhile costs nothing on the link. At most `cap`
/// bindings (default 4) linger; beyond, the least recently released go first. Types not listed do not linger.
#[derive(Clone, Debug)]
pub struct LingerOptions {
    pub types: HashMap<String, Duration>,
    pub cap: usize,
}

impl Default for LingerOptions {
    fn default() -> Self {
        Self { types: HashMap::new(), cap: 4 }
    }
}

impl HostImpl {
    pub(crate) fn kind(&self) -> PeerKind {
        self.kind
    }

    pub(crate) fn incarnation(&self) -> &str {
        &self.incarnation
    }

    pub(crate) fn error_handler(&self) -> ErrorHandler {
        self.on_error.clone()
    }

    pub(crate) fn weak(&self) -> Weak<HostImpl> {
        self.me.clone()
    }

    /// The task that owns this host's links, started on first use.
    pub(crate) fn net(&self) -> &NetHandle {
        self.net.get_or_init(|| spawn_net(self.me.clone(), self.handle.as_ref()))
    }

    pub(crate) fn bindings(&self) -> Vec<Arc<RemoteBinding>> {
        self.state().bindings.values().cloned().collect()
    }

    pub(crate) fn reloading(&self) -> bool {
        self.state().reloading
    }

    /// The oldest code version this host keeps.
    pub(crate) fn oldest_code_version(&self) -> i64 {
        let state = self.state();
        state.code_versions.keys().copied().min().unwrap_or(state.current)
    }

    /// The parent retired `code_version`: this host may retire it too (a reload seam).
    pub(crate) fn allow_retire(&self, code_version: u64) {
        {
            let mut state = self.state();
            state.retire_allowed = state.retire_allowed.max(code_version as i64);
        }
        self.maybe_retire();
    }

    /// The route for hosts named `host_name`: the one with the longest matching prefix.
    pub(crate) fn route_for(&self, host_name: &str) -> Option<Route> {
        let state = self.state();
        let matching = state.routes.iter().filter(|(prefix, _)| host_name.starts_with(prefix.as_str()));
        matching.max_by_key(|(prefix, _)| prefix.len()).map(|(_, route)| route.clone())
    }

    /// A reference to an object on host `at`: a view onto the binding for its key, created (and held) if there is
    /// none. `input` is normalized with `contract` (the hold carries it so).
    pub(super) fn acquire_remote(
        &self,
        contract: &Contract,
        at: String,
        input: Json,
        options: &GetOptions,
    ) -> ChordResult<Ref> {
        let principal = options.principal.clone().filter(|principal| !self.is_self(principal));
        let key = self.remote_key(contract, &input, principal.as_ref().unwrap_or(self.principal()));
        let (binding, fresh) = {
            let mut state = self.state();
            match state.bindings.get(&key) {
                Some(binding) if !binding.ended() => (binding.clone(), false),
                _ => {
                    let binding = RemoteBinding::create(self, key.clone(), contract.clone(), at, input, principal);
                    state.bindings.insert(key.clone(), binding.clone());
                    (binding, true)
                }
            }
        };
        let view = RemoteRef::new(binding.clone(), options.state != Some(false));
        self.state().lingering.shift_remove(&key);
        binding.add_view(&view);
        if fresh {
            self.net().post(Msg::Hold { binding });
        }
        Ok(Ref::new(view))
    }

    /// The last reference to `binding` was released: it lingers if its type does, else closes.
    pub(crate) fn binding_idle(&self, binding: &Arc<RemoteBinding>) {
        let duration = self.linger.types.get(binding.contract.name()).copied().unwrap_or_default();
        // A binding that never had a shape has nothing worth keeping.
        let handle = self.handle.clone().or_else(|| Handle::try_current().ok());
        let (true, false, true, true, Some(handle)) =
            (duration > Duration::ZERO, binding.ended(), binding.applied(), self.linger.cap > 0, handle)
        else {
            return binding.close();
        };
        let seq = self.linger_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let (armed, wait) = oneshot::channel::<()>();
        let weak = Arc::downgrade(binding);
        let timer = handle.spawn(async move {
            let _ = wait.await;
            tokio::time::sleep(duration).await;
            if let Some(binding) = weak.upgrade() {
                binding.linger_expired(seq);
            }
        });
        binding.set_linger(seq, timer.abort_handle());
        let _ = armed.send(());
        let evicted: Vec<Arc<RemoteBinding>> = {
            let mut state = self.state();
            state.lingering.insert(binding.key.clone(), binding.clone());
            let excess = state.lingering.len().saturating_sub(self.linger.cap);
            state.lingering.values().take(excess).cloned().collect()
        };
        evicted.iter().for_each(|oldest| oldest.close());
    }

    pub(crate) fn forget_binding(&self, binding: &Arc<RemoteBinding>) {
        let mut state = self.state();
        if state.bindings.get(&binding.key).is_some_and(|known| Arc::ptr_eq(known, binding)) {
            state.bindings.shift_remove(&binding.key);
        }
        if state.lingering.get(&binding.key).is_some_and(|known| Arc::ptr_eq(known, binding)) {
            state.lingering.shift_remove(&binding.key);
        }
    }

    /// The bindings lingering now (closed first when the host closes).
    pub(super) fn lingering(&self) -> Vec<Arc<RemoteBinding>> {
        self.state().lingering.values().cloned().collect()
    }

    pub(crate) fn connection_closed(&self, id: ConnId) {
        let foreign: Vec<String> = {
            let mut state = self.state();
            state.root_links.values_mut().for_each(|links| {
                links.remove(&id);
            });
            let came = state.foreign.iter().filter(|(_, links)| links.contains(&id));
            came.map(|(root_id, _)| root_id.clone()).collect()
        };
        // Foreign roots that came over this link will never announce their end over it.
        for root_id in foreign {
            self.foreign_root_ended(&root_id, id);
        }
    }

    /// Waits for the link task to finish a request (nothing to wait for without links).
    pub(super) fn ask_net(&self, request: impl FnOnce(oneshot::Sender<()>) -> Msg) -> BoxFuture<()> {
        let Some(net) = self.net.get() else { return Box::pin(std::future::ready(())) };
        let (done, wait) = oneshot::channel();
        net.post(request(done));
        Box::pin(async move {
            let _ = wait.await;
        })
    }
}

impl Host {
    pub fn kind(&self) -> PeerKind {
        self.0.kind
    }

    /// Accepts a child link (a worker or a browser); requests on it use the identity in `options`.
    pub fn accept(&self, link: Arc<dyn Link>, options: AcceptOptions) -> ChordResult<()> {
        if options.kind == PeerKind::Host && options.name.is_none() {
            return Err(ChordError::type_error("accepting a host link needs its name"));
        }
        self.0.net().post(Msg::Accept { link, options, host: self.0.clone() });
        Ok(())
    }

    /// Sets (or replaces, after a reconnect) the link to this host's parent; remote references re-hold over it.
    /// The parent's own requests use the identity in `options`; requests it relays use the one it forwards.
    pub fn connect(&self, link: Arc<dyn Link>, options: ConnectOptions) {
        self.0.net().post(Msg::Connect { link, options, host: self.0.clone() });
    }

    /// Where requests for hosts whose names start with `prefix` go.
    pub fn route(&self, prefix: &str, route: Route) {
        self.0.state().routes.insert(prefix.to_string(), route);
    }

    /// A snapshot of every open connection (diagnostics and tests).
    pub fn connections(&self) -> BoxFuture<Vec<ConnectionInfo>> {
        let Some(net) = self.0.net.get() else { return Box::pin(std::future::ready(Vec::new())) };
        let (reply, wait) = oneshot::channel();
        net.post(Msg::Inspect { reply });
        Box::pin(async move { wait.await.unwrap_or_default() })
    }
}
