//! [`RefImpl`], a reference on an object of this host. Members bind late, to the implementation current at
//! each access.

use std::sync::{Arc, Mutex, Weak};

use futures::FutureExt;
use indexmap::IndexMap;

use super::member::MemberReadable;
use super::resource::{AnyValue, Exposed};
use super::{Adapter, Ctx, Implementation, Installed, Member, ObjectImpl, RefCore, RefOwner, RootOptions, Service};
use super::{notify_status, run_root};
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{Contract, MemberDescription, Scope, bind, takes_no_args};
use crate::host2::errors::ChordErrorCode;
use crate::host2::exec::{guarded, in_factory, record_misuse};
use crate::host2::publish::enqueue;
use crate::host2::types::{ReadableRef, RefStatus, Shape, ShapeListener, StatusListener};
use crate::json::Json;
use crate::sync::{BoxFuture, SharedTask, Unsubscribe, lock};

type GoneListener = Arc<dyn Fn(&ChordError) + Send + Sync>;

struct RefState {
    status: RefStatus,
    error: Option<ChordError>,
    next: u64,
    shape_listeners: IndexMap<u64, ShapeListener>,
    status_listeners: IndexMap<u64, StatusListener>,
    /// Notified when the reference becomes gone (links forward it).
    gone_listeners: IndexMap<u64, GoneListener>,
    readables: IndexMap<String, Arc<MemberReadable>>,
    /// Bumped by every shape change: a link that sent a unit for a value skips older notifications.
    shape_seq: u64,
    /// Weak: a retired code version's service must not be kept alive by its consumers.
    bindings: Vec<(Weak<Service>, Option<Adapter>)>,
    views: Vec<(Weak<Implementation>, Arc<Implementation>)>,
}

/// One reference on an object of this host.
pub struct RefImpl {
    object: Arc<ObjectImpl>,
    contract: Contract,
    owner: RefOwner,
    ready: SharedTask,
    state: Mutex<RefState>,
    me: Weak<RefImpl>,
}

fn released(address: &str) -> ChordError {
    ChordError::code_only(ChordErrorCode::Released, format!("reference {address} released"))
}

fn failed<T: Send + 'static>(error: ChordError) -> BoxFuture<ChordResult<T>> {
    Box::pin(std::future::ready(Err(error)))
}

impl RefImpl {
    /// A reference on `object`; `live` when the object is live already. The object registers it.
    pub(crate) fn new(object: Arc<ObjectImpl>, contract: Contract, owner: RefOwner, live: bool) -> Arc<Self> {
        Arc::new_cyclic(|me: &Weak<Self>| {
            let object_ready = object.ready();
            let weak = me.clone();
            let address = object.address().to_string();
            let ready: BoxFuture<ChordResult<()>> = Box::pin(async move {
                object_ready.await?;
                let Some(reference) = weak.upgrade() else { return Ok(()) };
                let state = lock(&reference.state);
                match state.status {
                    RefStatus::Gone => Err(state.error.clone().unwrap_or_else(|| released(&address))),
                    RefStatus::Released => Err(released(&address)),
                    _ => Ok(()),
                }
            });
            let state = RefState {
                status: if live { RefStatus::Live } else { RefStatus::Loading },
                error: None,
                next: 0,
                shape_listeners: IndexMap::new(),
                status_listeners: IndexMap::new(),
                gone_listeners: IndexMap::new(),
                readables: IndexMap::new(),
                // A reference born live has its first shape already.
                shape_seq: u64::from(live),
                bindings: Vec::new(),
                views: Vec::new(),
            };
            Self { object, contract, owner, ready: ready.shared(), state: Mutex::new(state), me: me.clone() }
        })
    }

    pub(crate) fn object(&self) -> &Arc<ObjectImpl> {
        &self.object
    }

    pub fn owner(&self) -> RefOwner {
        self.owner
    }

    pub(crate) fn ready_task(&self) -> SharedTask {
        self.ready.clone()
    }

    pub(crate) fn id(&self) -> usize {
        self as *const Self as usize
    }

    /// Adds a listener to one of the listener maps; the returned handle removes it.
    fn listen<L: Send + 'static>(&self, map: fn(&mut RefState) -> &mut IndexMap<u64, L>, listener: L) -> Unsubscribe {
        let mut state = lock(&self.state);
        state.next += 1;
        let id = state.next;
        map(&mut state).insert(id, listener);
        let me = self.me.clone();
        Unsubscribe::new(move || {
            if let Some(me) = me.upgrade() {
                map(&mut lock(&me.state)).shift_remove(&id);
            }
        })
    }

    fn status_changed(&self) {
        let (listeners, status, error) = {
            let state = lock(&self.state);
            (state.status_listeners.values().cloned().collect(), state.status, state.error.clone())
        };
        notify_status(listeners, status, error, self.object.report());
    }

    fn readables(&self) -> Vec<Arc<MemberReadable>> {
        lock(&self.state).readables.values().cloned().collect()
    }

    /// The object went live.
    pub(crate) fn live(&self) {
        {
            let mut state = lock(&self.state);
            if state.status != RefStatus::Loading {
                return;
            }
            state.status = RefStatus::Live;
            state.shape_seq += 1;
        }
        self.status_changed();
        self.announce_shape();
    }

    pub(crate) fn gone(&self, error: &ChordError) {
        let listeners: Vec<GoneListener> = {
            let mut state = lock(&self.state);
            if state.status.ended() {
                return;
            }
            state.status = RefStatus::Gone;
            state.error = Some(error.clone());
            state.gone_listeners.values().cloned().collect()
        };
        self.readables().iter().for_each(|readable| readable.stop());
        self.status_changed();
        for listener in listeners {
            listener(error);
        }
    }

    /// Called with the error when the reference becomes gone (links forward it).
    pub fn on_gone(&self, listener: impl Fn(&ChordError) + Send + Sync + 'static) -> Unsubscribe {
        self.listen(|state| &mut state.gone_listeners, Arc::new(listener) as GoneListener)
    }

    /// The implementation changed (reload, variant): readables rebind, shape listeners hear it.
    pub(crate) fn shape_changed(&self) {
        lock(&self.state).shape_seq += 1;
        self.announce_shape();
    }

    /// The number of shape changes so far, when live (taken together with the status).
    pub(crate) fn live_seq(&self) -> Option<u64> {
        let state = lock(&self.state);
        (state.status == RefStatus::Live).then_some(state.shape_seq)
    }

    pub(crate) fn shape_seq(&self) -> u64 {
        lock(&self.state).shape_seq
    }

    fn announce_shape(&self) {
        self.readables().iter().for_each(|readable| readable.bind());
        let Some(shape) = self.shape() else { return };
        let listeners: Vec<ShapeListener> = lock(&self.state).shape_listeners.values().cloned().collect();
        let report = self.object.report();
        enqueue(move || {
            for listener in listeners {
                guarded(&report, || listener(&shape));
            }
        });
    }

    pub(crate) fn terminal_error(&self) -> ChordError {
        let state = lock(&self.state);
        match (&state.status, &state.error) {
            (RefStatus::Gone, Some(error)) => error.clone(),
            _ => released(self.object.address()),
        }
    }

    /// The implementation as this consumer sees it (through an accepts adapter when bound to another version).
    pub(crate) fn view_of(&self, installed: &Installed) -> ChordResult<Arc<Implementation>> {
        let service = &installed.service;
        let cached = {
            let state = lock(&self.state);
            let found = state.bindings.iter().find(|(key, _)| std::ptr::eq(key.as_ptr(), Arc::as_ptr(service)));
            found.map(|(_, adapter)| adapter.clone())
        };
        let adapter = match cached {
            Some(adapter) => adapter,
            None => {
                let adapter = bind(&service.contract, service.options.accepts.as_ref(), &self.contract)?.cloned();
                let mut state = lock(&self.state);
                state.bindings.retain(|(key, _)| key.strong_count() > 0);
                state.bindings.push((Arc::downgrade(service), adapter.clone()));
                adapter
            }
        };
        let Some(adapter) = adapter else { return Ok(installed.implementation.clone()) };
        let implementation = &installed.implementation;
        {
            let state = lock(&self.state);
            let found = state.views.iter().find(|(key, _)| std::ptr::eq(key.as_ptr(), Arc::as_ptr(implementation)));
            if let Some((_, view)) = found {
                return Ok(view.clone());
            }
        }
        let view = Arc::new(adapter(implementation));
        let mut state = lock(&self.state);
        state.views.retain(|(key, _)| key.strong_count() > 0);
        state.views.push((Arc::downgrade(implementation), view.clone()));
        Ok(view)
    }

    pub(crate) fn current_view(&self) -> Option<Arc<Implementation>> {
        if self.status() != RefStatus::Live {
            return None;
        }
        let installed = self.object.current_installed()?;
        self.safe_view(&installed)
    }

    /// The view, or `None` when this consumer no longer binds (reported; calls fail with the bind error).
    fn safe_view(&self, installed: &Installed) -> Option<Arc<Implementation>> {
        match self.view_of(installed) {
            Ok(view) => Some(view),
            Err(error) => {
                self.object.host().report(error);
                None
            }
        }
    }

    pub(crate) fn describe(&self, variant: Option<&str>, member: &str) -> Option<MemberDescription> {
        let variants = self.contract.description().variants();
        match variant {
            Some(variant) => variants.get(variant).and_then(|members| members.get(member)).cloned(),
            None => variants.values().find_map(|members| members.get(member)).cloned(),
        }
    }

    /// Remote consumers see only the members their contract lists for the current variant.
    pub(crate) fn in_variant(&self, view: &Implementation, name: &str) -> bool {
        self.contract.scope() != Scope::Remote || self.describe(view.variant_name(), name).is_some()
    }

    fn readable(&self, name: &str) -> ReadableRef {
        let mut state = lock(&self.state);
        let entry = state.readables.entry(name.to_string());
        entry.or_insert_with(|| MemberReadable::new(self.me.clone(), name)).clone()
    }
}

impl RefCore for RefImpl {
    fn address(&self) -> &str {
        self.object.address()
    }

    fn contract(&self) -> &Contract {
        &self.contract
    }

    fn status(&self) -> RefStatus {
        lock(&self.state).status
    }

    fn error(&self) -> Option<ChordError> {
        lock(&self.state).error.clone()
    }

    fn ready(&self) -> BoxFuture<ChordResult<()>> {
        Box::pin(self.ready.clone())
    }

    fn object(&self) -> Option<Arc<ObjectImpl>> {
        Some(self.object.clone())
    }

    fn variant(&self) -> Option<String> {
        self.current_view().and_then(|view| view.variant_name().map(str::to_string))
    }

    fn shape(&self) -> Option<Shape> {
        if self.status() != RefStatus::Live {
            return None;
        }
        let installed = self.object.current_installed()?;
        let view = self.safe_view(&installed)?;
        Some(Shape {
            variant: view.variant_name().map(str::to_string),
            members: view.names(),
            contract: installed.service.contract.fingerprint().to_string(),
        })
    }

    fn on_shape(&self, listener: ShapeListener) -> Unsubscribe {
        self.listen(|state| &mut state.shape_listeners, listener)
    }

    fn on_status(&self, listener: StatusListener) -> Unsubscribe {
        self.listen(|state| &mut state.status_listeners, listener)
    }

    fn release(&self) {
        {
            let mut state = lock(&self.state);
            if state.status.ended() {
                return;
            }
            state.status = RefStatus::Released;
        }
        self.readables().iter().for_each(|readable| readable.stop());
        self.status_changed();
        self.object.release_ref(self);
    }

    fn has(&self, name: &str) -> bool {
        if let Some(MemberDescription::Method { .. } | MemberDescription::State { .. }) = self.describe(None, name) {
            return true;
        }
        let Some(view) = self.current_view() else { return false };
        match view.get(name) {
            Some(Member::Method(_) | Member::State(_)) => true,
            Some(Member::Opaque(_)) => self.in_variant(&view, name),
            None => false,
        }
    }

    fn state(&self, name: &str) -> Option<ReadableRef> {
        let exposed = || self.current_view().is_some_and(|view| matches!(view.get(name), Some(Member::State(_))));
        match self.describe(None, name) {
            Some(MemberDescription::State { .. }) => Some(self.readable(name)),
            None if exposed() => Some(self.readable(name)),
            _ => None,
        }
    }

    fn opaque(&self, name: &str) -> ChordResult<Option<AnyValue>> {
        let Some(view) = self.current_view() else { return Ok(None) };
        if !self.in_variant(&view, name) {
            return Ok(None);
        }
        match view.get(name) {
            Some(Member::Opaque(value)) => match value.downcast_ref::<Exposed>() {
                Some(exposed) => exposed.resource.value().map(Some),
                None => Ok(Some(value.clone())),
            },
            _ => Ok(None),
        }
    }

    fn call(&self, name: &str, args: Option<Json>, ctx: Option<&Ctx>) -> BoxFuture<ChordResult<Option<Json>>> {
        let args = if takes_no_args(&self.contract, name) { None } else { args };
        let host = self.object.host().clone();
        if in_factory(host.id()) {
            let message = format!("{}.{name} called inside a factory", self.object.address());
            let error = ChordError::code_only(ChordErrorCode::NotReady, message);
            record_misuse(host.id(), error.clone());
            return failed(error);
        }
        let Some(me) = self.me.upgrade() else { return failed(released(self.object.address())) };
        let (object, name) = (self.object.clone(), name.to_string());
        match ctx {
            Some(ctx) if !ctx.holder() => object.call(me, name, args, ctx.clone()),
            _ => {
                // A code holder (task) starts a fresh root per call, cancelled with the holder.
                let mut options = RootOptions::default();
                if let Some(ctx) = ctx {
                    if let Err(error) = ctx.check(host.id()) {
                        return failed(error);
                    }
                    options.signal = Some(ctx.signal().clone());
                }
                run_root(&host, options, move |root| object.call(me, name, args, root))
            }
        }
    }
}
