//! Port of upstream `remote.ts`: one binding per host and key (type, input, identity key, contract fingerprint),
//! shared by every reference to it: one hold over a link, one stream per state member. A shape is applied once all
//! its snapshots are here; after the first hydration values stay readable while the binding re-holds.

mod drive;
mod view;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use indexmap::IndexMap;
use tokio::sync::{Notify, oneshot};

pub(crate) use view::{RemoteRef, ViewReadable};

use super::connection::{ConnId, Msg};
use super::contract::{Contract, MemberDescription};
use super::errors::ChordErrorCode;
use super::exec::ErrorHandler;
use super::host::HostImpl;
use super::publish::enqueue;
use super::types::{Shape, StateEvent};
use super::wire::{StateKind, WireError};
use crate::delta::Decoder;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{BoxFuture, lock};

/// One state frame for a member, as the binding receives it (payload unpacked).
pub(crate) struct StateIn {
    pub kind: StateKind,
    pub value: Option<Json>,
    pub ops: Option<Vec<Vec<Json>>>,
    pub error: Option<WireError>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Loading,
    Live,
    Ended,
}

/// A member's value with the decoder of its current stream.
struct MemberState {
    value: Json,
    decoder: Decoder,
}

/// A shape waiting for its snapshots.
struct PendingShape {
    shape: Shape,
    missing: Vec<String>,
    values: IndexMap<String, MemberState>,
}

struct BindingState {
    status: Status,
    error: Option<ChordError>,
    /// The applied shape (kept while re-holding).
    shape: Option<Shape>,
    /// Every state member of the applied shape has a value (stays true while re-holding).
    hydrated: bool,
    /// The current hold; `None` while resolving the route or after a link loss.
    hold: Option<(ConnId, u64)>,
    /// What the provider was told: state for this hold (`state: false` / `sub` / `unsub`).
    state_on: bool,
    views: Vec<Arc<RemoteRef>>,
    members: HashMap<String, MemberState>,
    linger: Option<(u64, tokio::task::AbortHandle)>,
    listeners: HashMap<String, Vec<Arc<ViewReadable>>>,
    pending: Option<PendingShape>,
    /// A shape was applied once (a re-hold says `again`).
    applied: bool,
    hydration_waiters: Vec<oneshot::Sender<ChordResult<()>>>,
}

impl BindingState {
    fn state_views(&self) -> usize {
        self.views.iter().filter(|view| view.wants_state()).count()
    }
}

/// Work a change leaves for after the lock: view updates and deliveries keep their order through the queue.
enum After {
    Update(Vec<Arc<RemoteRef>>),
    Deliver(Vec<Arc<ViewReadable>>, StateEvent),
    ShapeChanged(Vec<Arc<RemoteRef>>),
    End(Vec<Arc<RemoteRef>>, ChordError),
    Settle(Vec<oneshot::Sender<ChordResult<()>>>, ChordResult<()>),
    Post(Msg),
    Forget,
}

pub(crate) struct RemoteBinding {
    host: Weak<HostImpl>,
    report: ErrorHandler,
    pub key: String,
    pub contract: Contract,
    pub at: String,
    pub input: Json,
    /// Forwarded identity (`None` for the host's own).
    pub principal: Option<Json>,
    state: Mutex<BindingState>,
    /// Notified at every status or hold change (calls wait for a live hold).
    changed: Notify,
    me: Weak<RemoteBinding>,
}

impl RemoteBinding {
    pub(crate) fn create(
        host: &HostImpl,
        key: String,
        contract: Contract,
        at: String,
        input: Json,
        principal: Option<Json>,
    ) -> Arc<Self> {
        let state = BindingState {
            status: Status::Loading,
            error: None,
            shape: None,
            hydrated: false,
            hold: None,
            state_on: false,
            views: Vec::new(),
            members: HashMap::new(),
            linger: None,
            listeners: HashMap::new(),
            pending: None,
            applied: false,
            hydration_waiters: Vec::new(),
        };
        Arc::new_cyclic(|me| Self {
            host: host.weak(),
            report: host.error_handler(),
            key,
            contract,
            at,
            input,
            principal,
            state: Mutex::new(state),
            changed: Notify::new(),
            me: me.clone(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, BindingState> {
        lock(&self.state)
    }

    pub(crate) fn report(&self, error: ChordError) {
        (self.report)(error);
    }

    pub(crate) fn ended(&self) -> bool {
        self.lock().status == Status::Ended
    }

    /// Whether a view is live: a shape over a current hold, and the state if the view wants it.
    fn live_for(&self, wants_state: bool) -> bool {
        let state = self.lock();
        state.status == Status::Live && state.hold.is_some() && (!wants_state || state.hydrated)
    }

    pub(crate) fn hold(&self) -> Option<(ConnId, u64)> {
        self.lock().hold
    }

    pub(crate) fn applied(&self) -> bool {
        self.lock().applied
    }

    pub(crate) fn hydrated(&self) -> bool {
        self.lock().hydrated
    }

    pub(crate) fn shape(&self) -> Option<Shape> {
        self.lock().shape.clone()
    }

    fn released(&self) -> ChordError {
        ChordError::code_only(ChordErrorCode::Released, format!("reference {} released", self.key))
    }

    fn terminal(&self, state: &BindingState) -> ChordError {
        state.error.clone().unwrap_or_else(|| self.released())
    }

    fn run(&self, after: Vec<After>) {
        self.changed.notify_waiters();
        for work in after {
            match work {
                After::Update(views) => views.iter().for_each(|view| view.update()),
                After::Deliver(readables, event) => enqueue(move || {
                    readables.iter().for_each(|readable| readable.deliver(&event));
                }),
                After::ShapeChanged(views) => views.iter().for_each(|view| view.shape_changed()),
                After::End(views, error) => views.iter().for_each(|view| view.end(&error)),
                After::Settle(waiters, result) => {
                    for waiter in waiters {
                        let _ = waiter.send(result.clone());
                    }
                }
                After::Post(msg) => {
                    if let Some(host) = self.host.upgrade() {
                        host.net().post(msg);
                    }
                }
                After::Forget => {
                    if let (Some(host), Some(me)) = (self.host.upgrade(), self.me.upgrade()) {
                        host.forget_binding(&me);
                    }
                }
            }
        }
    }

    // Views.

    pub(crate) fn add_view(&self, view: &Arc<RemoteRef>) {
        {
            let mut state = self.lock();
            state.views.push(view.clone());
            if let Some((_, timer)) = state.linger.take() {
                timer.abort();
            }
        }
        self.sync_state();
        view.update();
    }

    pub(crate) fn remove_view(&self, view: &RemoteRef) {
        let idle = {
            let mut state = self.lock();
            let Some(index) = state.views.iter().position(|known| std::ptr::eq(Arc::as_ptr(known), view)) else {
                return;
            };
            state.views.remove(index);
            for readables in state.listeners.values_mut() {
                readables.retain(|readable| !readable.of(view));
            }
            state.views.is_empty()
        };
        match (idle, self.host.upgrade(), self.me.upgrade()) {
            (true, Some(host), Some(me)) => host.binding_idle(&me),
            (true, _, _) => self.close(),
            (false, _, _) => self.sync_state(),
        }
    }

    /// Tells the provider to start or stop state when the views' wish changed.
    fn sync_state(&self) {
        let after = {
            let mut state = self.lock();
            if state.status == Status::Ended || state.views.is_empty() {
                return;
            }
            let wanted = state.state_views() > 0;
            if wanted == state.state_on {
                return;
            }
            state.state_on = wanted;
            if !wanted {
                state.hydrated = false;
                state.members.clear();
                if let Some(pending) = state.pending.as_mut() {
                    pending.missing.clear();
                }
            }
            let mut after = Vec::new();
            if let Some((conn, hold)) = state.hold {
                after.push(After::Post(Msg::WantState { conn, hold, wanted }));
            }
            after.push(After::Update(state.views.clone()));
            after
        };
        self.run(after);
    }

    pub(crate) fn listen(&self, name: &str, readable: &Arc<ViewReadable>) {
        let mut state = self.lock();
        let readables = state.listeners.entry(name.to_string()).or_default();
        if !readables.iter().any(|known| Arc::ptr_eq(known, readable)) {
            readables.push(readable.clone());
        }
    }

    pub(crate) fn unlisten(&self, name: &str, readable: &ViewReadable) {
        if let Some(readables) = self.lock().listeners.get_mut(name) {
            readables.retain(|known| !std::ptr::eq(Arc::as_ptr(known), readable));
        }
    }

    /// Whether `name` is a state member of the applied variant (as the consumer's contract describes it).
    fn in_variant(&self, state: &BindingState, name: &str) -> bool {
        let variant = state.shape.as_ref().and_then(|shape| shape.variant.as_deref());
        let member = variant.and_then(|variant| self.contract.description().variant(variant)?.get(name));
        matches!(member, Some(MemberDescription::State { .. }))
    }

    /// A member's value as views read it: `not_ready` before the first hydration, `not_in_variant` outside it.
    pub(crate) fn read(&self, name: &str) -> ChordResult<Json> {
        let state = self.lock();
        if state.status == Status::Ended {
            return Err(self.terminal(&state));
        }
        if !state.hydrated {
            let message = format!("{}.{name} is not hydrated yet", self.key);
            return Err(ChordError::code_only(ChordErrorCode::NotReady, message));
        }
        if !self.in_variant(&state, name) {
            let variant = state.shape.as_ref().and_then(|shape| shape.variant.clone()).unwrap_or_default();
            let message = format!("{}: {name} is not a state member of variant {variant}", self.key);
            return Err(ChordError::code_only(ChordErrorCode::NotInVariant, message));
        }
        Ok(state.members.get(name).map(|member| member.value.clone()).unwrap_or_default())
    }

    /// The value a new listener gets at once, if the member is readable now.
    pub(crate) fn readable_value(&self, name: &str) -> Option<Json> {
        let state = self.lock();
        let readable = state.status != Status::Ended && state.hydrated && self.in_variant(&state, name);
        readable.then(|| state.members.get(name).map(|member| member.value.clone()).unwrap_or_default())
    }

    /// Resolves once the binding has its state (fails if it ends first).
    pub(crate) fn when_hydrated(&self) -> BoxFuture<ChordResult<()>> {
        let mut state = self.lock();
        if state.hydrated {
            return Box::pin(std::future::ready(Ok(())));
        }
        if state.status == Status::Ended {
            return Box::pin(std::future::ready(Err(self.terminal(&state))));
        }
        let (sender, receiver) = oneshot::channel();
        state.hydration_waiters.push(sender);
        let released = self.released();
        Box::pin(async move { receiver.await.unwrap_or(Err(released)) })
    }

    /// Lingering since the last view went (set by the host); a view taken meanwhile stops it.
    pub(crate) fn set_linger(&self, seq: u64, timer: tokio::task::AbortHandle) {
        let mut state = self.lock();
        if let Some((_, previous)) = state.linger.replace((seq, timer)) {
            previous.abort();
        }
    }

    pub(crate) fn linger_expired(&self, seq: u64) {
        let expired = {
            let state = self.lock();
            state.views.is_empty() && state.linger.as_ref().is_some_and(|(current, _)| *current == seq)
        };
        if expired {
            self.close();
        }
    }
}
