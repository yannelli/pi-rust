//! [`RemoteRef`], a reference on an object of another host: a view onto the host's shared binding for its key.
//! Each view has its own status, observers and `release`; the provider ending it and re-holding belong to
//! the binding. [`ViewReadable`] is a view's state member.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use futures::FutureExt;
use indexmap::IndexMap;
use tokio::sync::oneshot;

use super::RemoteBinding;
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{Contract, MemberDescription, takes_no_args};
use crate::host2::errors::ChordErrorCode;
use crate::host2::exec::{guarded, in_factory, record_misuse};
use crate::host2::object::{AnyValue, Ctx, HostCore, ObjectImpl, RefCore, RootOptions, notify_status, run_root};
use crate::host2::publish::enqueue;
use crate::host2::types::{
    EventListener, Readable, ReadableRef, RefStatus, Shape, ShapeListener, StateEvent, StatusListener, ValueListener,
};
use crate::json::Json;
use crate::sync::{BoxFuture, SharedTask, Unsubscribe, lock};

fn failed<T: Send + 'static>(error: ChordError) -> BoxFuture<ChordResult<T>> {
    Box::pin(std::future::ready(Err(error)))
}

struct ViewState {
    status: RefStatus,
    error: Option<ChordError>,
    settle: Option<oneshot::Sender<ChordResult<()>>>,
    next: u64,
    shape_listeners: IndexMap<u64, ShapeListener>,
    status_listeners: IndexMap<u64, StatusListener>,
    readables: IndexMap<String, Arc<ViewReadable>>,
}

pub(crate) struct RemoteRef {
    binding: Arc<RemoteBinding>,
    /// False: a method-only view (`state: false`), ready once live.
    wants_state: bool,
    ready: SharedTask,
    state: Mutex<ViewState>,
    me: Weak<RemoteRef>,
}

impl RemoteRef {
    pub(crate) fn new(binding: Arc<RemoteBinding>, wants_state: bool) -> Arc<Self> {
        let (settle, settled) = oneshot::channel();
        let released = binding.released();
        let ready: BoxFuture<ChordResult<()>> = Box::pin(async move { settled.await.unwrap_or(Err(released)) });
        let state = ViewState {
            status: RefStatus::Loading,
            error: None,
            settle: Some(settle),
            next: 0,
            shape_listeners: IndexMap::new(),
            status_listeners: IndexMap::new(),
            readables: IndexMap::new(),
        };
        Arc::new_cyclic(|me| Self {
            binding,
            wants_state,
            ready: ready.shared(),
            state: Mutex::new(state),
            me: me.clone(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, ViewState> {
        lock(&self.state)
    }

    pub(crate) fn wants_state(&self) -> bool {
        self.wants_state
    }

    fn ended(&self) -> bool {
        self.lock().status.ended()
    }

    fn terminal_error(&self) -> ChordError {
        let state = self.lock();
        match (state.status, &state.error) {
            (RefStatus::Gone, Some(error)) => error.clone(),
            _ => self.binding.released(),
        }
    }

    fn notify(&self) {
        let (listeners, status, error) = {
            let state = self.lock();
            (state.status_listeners.values().cloned().collect(), state.status, state.error.clone())
        };
        notify_status(listeners, status, error, self.binding.report.clone());
    }

    fn stop_readables(&self) {
        let readables: Vec<_> = self.lock().readables.values().cloned().collect();
        readables.iter().for_each(|readable| readable.stop());
    }

    /// Follows the binding: live once it has a shape (and, for a state view, its state) over a current hold.
    pub(crate) fn update(&self) {
        let live = self.binding.live_for(self.wants_state);
        let settle = {
            let mut state = self.lock();
            let next = if live { RefStatus::Live } else { RefStatus::Loading };
            if state.status.ended() || state.status == next {
                return;
            }
            state.status = next;
            if live { state.settle.take() } else { None }
        };
        self.notify();
        if let Some(settle) = settle {
            let _ = settle.send(Ok(()));
        }
    }

    pub(crate) fn end(&self, error: &ChordError) {
        let settle = {
            let mut state = self.lock();
            if state.status.ended() {
                return;
            }
            let released = error.code() == Some(ChordErrorCode::Released);
            state.status = if released { RefStatus::Released } else { RefStatus::Gone };
            if !released {
                state.error = Some(error.clone());
            }
            state.settle.take()
        };
        self.notify();
        if let Some(settle) = settle {
            let _ = settle.send(Err(error.clone()));
        }
        self.stop_readables();
    }

    pub(crate) fn shape_changed(&self) {
        let Some(shape) = self.binding.shape() else { return };
        let listeners: Vec<ShapeListener> = {
            let state = self.lock();
            if state.status.ended() {
                return;
            }
            state.shape_listeners.values().cloned().collect()
        };
        let report = self.binding.report.clone();
        enqueue(move || {
            for listener in listeners {
                guarded(&report, || listener(&shape));
            }
        });
    }

    fn listen<L>(&self, map: fn(&mut ViewState) -> &mut IndexMap<u64, L>, listener: L) -> Unsubscribe
    where
        L: Send + 'static,
    {
        let mut state = self.lock();
        state.next += 1;
        let id = state.next;
        map(&mut state).insert(id, listener);
        let me = self.me.clone();
        Unsubscribe::new(move || {
            if let Some(me) = me.upgrade() {
                map(&mut me.lock()).shift_remove(&id);
            }
        })
    }

    /// How the consumer's contract describes `name`, in any variant.
    fn describe(&self, name: &str) -> Option<&MemberDescription> {
        self.binding.contract.description().variants().values().find_map(|members| members.get(name))
    }

    fn readable(&self, name: &str) -> Arc<ViewReadable> {
        let mut state = self.lock();
        let entry = state.readables.entry(name.to_string());
        entry.or_insert_with(|| ViewReadable::new(self.me.clone(), self.binding.clone(), name)).clone()
    }

    fn call_in(&self, name: String, args: Option<Json>, ctx: Ctx) -> BoxFuture<ChordResult<Option<Json>>> {
        if self.ended() {
            return failed(self.terminal_error());
        }
        Box::pin(self.binding.clone().call(name, args, ctx))
    }
}

impl RefCore for RemoteRef {
    fn address(&self) -> &str {
        &self.binding.key
    }

    fn contract(&self) -> &Contract {
        &self.binding.contract
    }

    fn status(&self) -> RefStatus {
        self.lock().status
    }

    fn error(&self) -> Option<ChordError> {
        self.lock().error.clone()
    }

    fn ready(&self) -> BoxFuture<ChordResult<()>> {
        Box::pin(self.ready.clone())
    }

    fn object(&self) -> Option<Arc<ObjectImpl>> {
        None
    }

    fn variant(&self) -> Option<String> {
        self.binding.shape().and_then(|shape| shape.variant)
    }

    fn shape(&self) -> Option<Shape> {
        self.binding.shape()
    }

    fn on_shape(&self, listener: ShapeListener) -> Unsubscribe {
        self.listen(|state| &mut state.shape_listeners, listener)
    }

    fn on_status(&self, listener: StatusListener) -> Unsubscribe {
        self.listen(|state| &mut state.status_listeners, listener)
    }

    fn release(&self) {
        let settle = {
            let mut state = self.lock();
            if state.status.ended() {
                return;
            }
            state.status = RefStatus::Released;
            state.settle.take()
        };
        self.notify();
        if let Some(settle) = settle {
            let _ = settle.send(Err(self.binding.released()));
        }
        self.stop_readables();
        self.binding.remove_view(self);
    }

    fn has(&self, name: &str) -> bool {
        matches!(self.describe(name), Some(MemberDescription::Method { .. } | MemberDescription::State { .. }))
    }

    fn state(&self, name: &str) -> Option<ReadableRef> {
        match self.describe(name) {
            Some(MemberDescription::State { .. }) => Some(self.readable(name)),
            _ => None,
        }
    }

    fn opaque(&self, _name: &str) -> ChordResult<Option<AnyValue>> {
        Ok(None)
    }

    fn call(&self, name: &str, args: Option<Json>, ctx: Option<&Ctx>) -> BoxFuture<ChordResult<Option<Json>>> {
        let args = if takes_no_args(&self.binding.contract, name) { None } else { args };
        let Some(host) = self.binding.host.upgrade() else { return failed(self.binding.released()) };
        if in_factory(host.id()) {
            let message = format!("{}.{name} called inside a factory", self.address());
            let error = ChordError::code_only(ChordErrorCode::NotReady, message);
            record_misuse(host.id(), error.clone());
            return failed(error);
        }
        // The same checks as a local reference: a context of another host would carry a root it does not own.
        if let Some(Err(error)) = ctx.map(|ctx| ctx.check(host.id())) {
            return failed(error);
        }
        let Some(me) = self.me.upgrade() else { return failed(self.binding.released()) };
        let name = name.to_string();
        match ctx {
            Some(ctx) if !ctx.holder() => self.call_in(name, args, ctx.clone()),
            _ => {
                let options = RootOptions { signal: ctx.map(|ctx| ctx.signal().clone()), ..RootOptions::default() };
                run_root(&host.core(), options, move |root| me.call_in(name, args, root))
            }
        }
    }
}

#[derive(Default)]
struct ReadableState {
    next: u64,
    values: IndexMap<u64, ValueListener>,
    events: IndexMap<u64, EventListener>,
    /// Event listeners that have not had their snapshot yet.
    unseen: HashSet<u64>,
}

/// A state member of a remote reference: the binding's value. Reading fails with `not_ready` before the first
/// hydration and `not_in_variant` outside the variant. A listener added before the hydration gets its snapshot then.
pub(crate) struct ViewReadable {
    view: Weak<RemoteRef>,
    binding: Arc<RemoteBinding>,
    name: String,
    state: Mutex<ReadableState>,
    me: Weak<ViewReadable>,
}

impl ViewReadable {
    fn new(view: Weak<RemoteRef>, binding: Arc<RemoteBinding>, name: &str) -> Arc<Self> {
        let name = name.to_string();
        Arc::new_cyclic(|me| Self { view, binding, name, state: Mutex::default(), me: me.clone() })
    }

    /// Whether this readable belongs to `view`.
    pub(crate) fn of(&self, view: &RemoteRef) -> bool {
        std::ptr::eq(self.view.as_ptr(), view)
    }

    fn view_ended(&self) -> bool {
        self.view.upgrade().is_none_or(|view| view.ended())
    }

    fn safe(&self, listener: impl FnOnce()) {
        guarded(&self.binding.report, listener);
    }

    /// The value a new listener gets at once, if readable now.
    fn readable_value(&self) -> Option<Json> {
        if self.view_ended() { None } else { self.binding.readable_value(&self.name) }
    }

    fn attach(&self) {
        if let (false, Some(me)) = (self.view_ended(), self.me.upgrade()) {
            self.binding.listen(&self.name, &me);
        }
    }

    fn unsubscriber(&self, id: u64) -> Unsubscribe {
        let me = self.me.clone();
        Unsubscribe::new(move || {
            let Some(me) = me.upgrade() else { return };
            let idle = {
                let mut state = lock(&me.state);
                state.unseen.remove(&id);
                state.values.shift_remove(&id);
                state.events.shift_remove(&id);
                state.values.is_empty() && state.events.is_empty()
            };
            if idle {
                me.binding.unlisten(&me.name, &me);
            }
        })
    }

    pub(crate) fn deliver(&self, event: &StateEvent) {
        let (events, values) = {
            let mut state = lock(&self.state);
            let ids: Vec<u64> = state.events.keys().copied().collect();
            let unseen: Vec<bool> = ids.iter().map(|id| state.unseen.remove(id)).collect();
            let events: Vec<_> = state.events.values().cloned().zip(unseen).collect();
            (events, state.values.values().cloned().collect::<Vec<_>>())
        };
        for (listener, unseen) in events {
            if !unseen {
                self.safe(|| listener(event));
            } else if !matches!(event, StateEvent::Ops { .. }) {
                let snap = StateEvent::Snap { value: event.value().clone() };
                self.safe(|| listener(&snap));
            }
        }
        for listener in values {
            self.safe(|| listener(event.value()));
        }
    }

    pub(crate) fn stop(&self) {
        {
            let mut state = lock(&self.state);
            state.values.clear();
            state.events.clear();
            state.unseen.clear();
        }
        self.binding.unlisten(&self.name, self);
    }
}

impl Readable for ViewReadable {
    fn value(&self) -> ChordResult<Json> {
        if let Some(view) = self.view.upgrade()
            && view.ended()
        {
            return Err(view.terminal_error());
        }
        self.binding.read(&self.name)
    }

    fn subscribe(&self, listener: ValueListener) -> Unsubscribe {
        let id = {
            let mut state = lock(&self.state);
            state.next += 1;
            let id = state.next;
            state.values.insert(id, listener.clone());
            id
        };
        self.attach();
        if let Some(value) = self.readable_value() {
            self.safe(|| listener(&value));
        }
        self.unsubscriber(id)
    }

    fn events(&self, listener: EventListener) -> Unsubscribe {
        let id = {
            let mut state = lock(&self.state);
            state.next += 1;
            let id = state.next;
            state.events.insert(id, listener.clone());
            id
        };
        self.attach();
        match self.readable_value() {
            Some(value) => self.safe(|| listener(&StateEvent::Snap { value })),
            None => {
                lock(&self.state).unseen.insert(id);
            }
        }
        self.unsubscriber(id)
    }

    /// Not hydrated before the binding's first snapshots (a local object forwarding this member waits for them).
    fn hydrated(&self) -> bool {
        self.binding.hydrated()
    }

    fn when_hydrated(&self) -> BoxFuture<ChordResult<()>> {
        self.binding.when_hydrated()
    }
}
