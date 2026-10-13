//! Consumer bindings: stable facades over a transport for an allowlist of services.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;

use super::facade::ServiceFacade;
use super::keyed::KeyedBinding;
use crate::context::{BACKGROUND_CONTEXT, Context, await_with_context};
use crate::error::{ChordError, ChordResult, RemoteServiceErrorCode as Code};
use crate::services::handle::{AccessCheck, ServiceHandle, always_allowed};
use crate::services::provider_subscriber::ServiceUpdateListener;
use crate::services::remote::{ObserveHandler, RemoteServices};
use crate::services::token::ServiceRef;
use crate::services::transport::{RemoteServiceTransport, ServiceSubscription};
use crate::services::validate::validate_reset_snapshot;
use crate::services::wire::{ServiceMode, ServiceProviderUpdate, ServiceProviderUpdateOf};
use crate::state::{ErrorReporter, service_delivery_context};
use crate::sync::{
    BoxFuture, SharedTask, Unsubscribe, lock, ready_task, settle_all, spawn_eager, start_shared, yield_now,
};

/// How to build a [`RemoteServiceBinding`].
#[derive(Clone)]
pub struct RemoteServiceBindingOptions {
    pub services: Vec<ServiceRef>,
    pub transport: Arc<dyn RemoteServiceTransport>,
    /// Whether to subscribe right away (default true).
    pub bound: bool,
    /// Receives errors that have no caller. Defaults to dropping them, as upstream does.
    pub on_error: Option<ErrorReporter>,
    pub assert_access: Option<AccessCheck>,
}

impl RemoteServiceBindingOptions {
    pub fn new(services: Vec<ServiceRef>, transport: Arc<dyn RemoteServiceTransport>) -> Self {
        Self { services, transport, bound: true, on_error: None, assert_access: None }
    }
}

#[derive(Default)]
struct SingletonState {
    active: bool,
    revision: u64,
    starting: Option<SharedTask>,
    subscription: Option<Arc<dyn ServiceSubscription>>,
}

struct Singleton {
    facade: Arc<ServiceFacade>,
    state: Mutex<SingletonState>,
}

struct State {
    modes: HashMap<String, ServiceMode>,
    singletons: HashMap<String, Arc<Singleton>>,
    keyed: HashMap<String, Arc<KeyedBinding>>,
    readiness_revision: u64,
    transition: SharedTask,
}

struct Inner {
    transport: Arc<dyn RemoteServiceTransport>,
    allowlist: HashSet<String>,
    report_error: ErrorReporter,
    assert_access: AccessCheck,
    bound: AtomicBool,
    disposed: AtomicBool,
    state: Mutex<State>,
    me: Weak<Inner>,
}

/// Binds an allowlist of remote services through a transport and can be rebound in place.
#[derive(Clone)]
pub struct RemoteServiceBinding(Arc<Inner>);

/// Create a binding. Duplicate service IDs are a type error.
pub fn create_remote_service_binding(options: RemoteServiceBindingOptions) -> ChordResult<RemoteServiceBinding> {
    let allowlist: HashSet<String> = options.services.iter().map(|service| service.id().to_string()).collect();
    if allowlist.len() != options.services.len() {
        return Err(ChordError::type_error("Remote service binding has duplicate service IDs"));
    }
    Ok(RemoteServiceBinding(Arc::new_cyclic(|me| Inner {
        transport: options.transport,
        allowlist,
        report_error: options.on_error.unwrap_or_else(|| Arc::new(|_| {})),
        assert_access: options.assert_access.unwrap_or_else(always_allowed),
        bound: AtomicBool::new(options.bound),
        disposed: AtomicBool::new(false),
        state: Mutex::new(State {
            modes: HashMap::new(),
            singletons: HashMap::new(),
            keyed: HashMap::new(),
            readiness_revision: 0,
            transition: ready_task(Ok(())),
        }),
        me: me.clone(),
    })))
}

fn disposed_error() -> ChordError {
    ChordError::error("Remote service binding is disposed")
}

impl Inner {
    fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    fn handle_access(&self) -> AccessCheck {
        let weak = self.me.clone();
        Arc::new(move || {
            let inner = weak.upgrade().ok_or_else(disposed_error)?;
            if inner.is_disposed() {
                return Err(disposed_error());
            }
            (inner.assert_access)()
        })
    }

    fn assert_remotable(service: &ServiceRef) -> ChordResult<()> {
        if service.is_local() {
            return Err(ChordError::remote(
                Code::ServiceNotAllowed,
                format!("Service {} is process-local", service.id()),
            ));
        }
        Ok(())
    }

    fn assert_available(&self, state: &mut State, id: &str, mode: ServiceMode) -> ChordResult<()> {
        if self.is_disposed() {
            return Err(disposed_error());
        }
        if !self.allowlist.contains(id) {
            return Err(ChordError::remote(Code::ServiceNotAllowed, format!("Remote service {id} is not allowlisted")));
        }
        if let Some(existing) = state.modes.get(id).filter(|existing| **existing != mode) {
            let message = format!("Remote service {id} is already used as {}", existing.as_str());
            return Err(ChordError::remote(Code::ServiceModeMismatch, message));
        }
        state.modes.insert(id.to_string(), mode);
        Ok(())
    }

    fn bump_readiness(&self) {
        lock(&self.state).readiness_revision += 1;
    }

    fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceHandle> {
        Self::assert_remotable(service)?;
        let id = service.id().to_string();
        let binding = {
            let mut state = lock(&self.state);
            self.assert_available(&mut state, &id, ServiceMode::Singleton)?;
            if let Some(binding) = state.singletons.get(&id) {
                return Ok(ServiceHandle(binding.facade.clone()));
            }
            let weak_binding: Arc<Mutex<Weak<Singleton>>> = Arc::new(Mutex::new(Weak::new()));
            let (slot, inner) = (weak_binding.clone(), self.me.clone());
            let facade = ServiceFacade::new(
                &id,
                None,
                self.transport.clone(),
                Arc::new(move || {
                    let active = lock(&slot).upgrade().is_some_and(|binding| lock(&binding.state).active);
                    active
                        && inner
                            .upgrade()
                            .is_some_and(|inner| !inner.is_disposed() && inner.bound.load(Ordering::SeqCst))
                }),
                self.handle_access(),
                self.report_error.clone(),
            );
            let state_init = SingletonState { active: true, ..SingletonState::default() };
            let binding = Arc::new(Singleton { facade, state: Mutex::new(state_init) });
            *lock(&weak_binding) = Arc::downgrade(&binding);
            state.singletons.insert(id.clone(), binding.clone());
            state.readiness_revision += 1;
            binding
        };
        if self.bound.load(Ordering::SeqCst) {
            let task = self.start_singleton_reporting(id, binding.clone(), 0);
            let mut state = lock(&binding.state);
            if state.revision == 0 {
                state.starting = Some(task);
            }
        }
        // A ready() that read the revision before the start task was stored loops once more.
        self.bump_readiness();
        Ok(ServiceHandle(binding.facade.clone()))
    }

    fn start_singleton_reporting(&self, id: String, binding: Arc<Singleton>, revision: u64) -> SharedTask {
        let weak = self.me.clone();
        start_shared(async move {
            let Some(inner) = weak.upgrade() else { return Ok(()) };
            let result = inner.start_singleton(id, binding.clone(), revision).await;
            if let Err(error) = &result {
                let current = {
                    let state = lock(&binding.state);
                    state.active && state.revision == revision
                };
                if current && !inner.is_disposed() && inner.bound.load(Ordering::SeqCst) {
                    (inner.report_error)(error.clone());
                }
            }
            result
        })
    }

    async fn start_singleton(&self, id: String, binding: Arc<Singleton>, revision: u64) -> ChordResult<()> {
        let (target, report, service_id) = (Arc::downgrade(&binding), self.report_error.clone(), id.clone());
        let listener: ServiceUpdateListener = Arc::new(move |update, context| {
            let Some(binding) = target.upgrade() else { return Ok(()) };
            {
                let state = lock(&binding.state);
                if !state.active || state.revision != revision {
                    return Ok(());
                }
            }
            if let Err(error) = apply_singleton_update(&binding.facade, &service_id, update, context) {
                report(error);
            }
            Ok(())
        });
        let subscription = self.transport.subscribe(&id, ServiceMode::Singleton, listener, &BACKGROUND_CONTEXT).await?;
        // Installation continues on a later turn, as after upstream's await on the subscription.
        yield_now().await;
        let current = {
            let mut state = lock(&binding.state);
            let current = state.active && state.revision == revision;
            let current = current && !self.is_disposed() && self.bound.load(Ordering::SeqCst);
            if current {
                state.subscription = Some(subscription.clone());
            }
            current
        };
        if !current {
            return subscription.close(&BACKGROUND_CONTEXT).await;
        }
        let snapshot = subscription.snapshot();
        if snapshot.mode != ServiceMode::Singleton || snapshot.service_id != id || snapshot.instances.len() != 1 {
            return Err(ChordError::error(format!("Remote service {id} returned an invalid singleton snapshot")));
        }
        binding.facade.install(&snapshot.instances[0], &service_delivery_context())?;
        subscription.activate()
    }

    fn observe(&self, service: &ServiceRef, handler: ObserveHandler) -> ChordResult<Unsubscribe> {
        Self::assert_remotable(service)?;
        let id = service.id().to_string();
        let binding = {
            let mut state = lock(&self.state);
            self.assert_available(&mut state, &id, ServiceMode::Keyed)?;
            match state.keyed.get(&id) {
                Some(binding) => binding.clone(),
                None => {
                    let binding = KeyedBinding::new(
                        &id,
                        self.transport.clone(),
                        self.report_error.clone(),
                        self.handle_access(),
                        self.on_empty(id.clone()),
                        self.bound.load(Ordering::SeqCst),
                    );
                    state.keyed.insert(id.clone(), binding.clone());
                    state.readiness_revision += 1;
                    binding
                }
            }
        };
        let stop = binding.observe(handler);
        self.bump_readiness();
        stop
    }

    fn on_empty(&self, id: String) -> super::keyed::OnEmpty {
        let weak = self.me.clone();
        Box::new(move |binding: &Arc<KeyedBinding>| {
            let Some(inner) = weak.upgrade() else { return };
            {
                let mut state = lock(&inner.state);
                if !state.keyed.get(&id).is_some_and(|current| Arc::ptr_eq(current, binding)) {
                    return;
                }
                state.keyed.remove(&id);
                state.readiness_revision += 1;
            }
            let (binding, report) = (binding.clone(), inner.report_error.clone());
            spawn_eager(async move {
                if let Err(error) = binding.close(&BACKGROUND_CONTEXT).await {
                    report(error);
                }
            });
        })
    }

    async fn ready(&self, context: &Context) -> ChordResult<()> {
        if self.is_disposed() {
            return Err(disposed_error());
        }
        loop {
            let (revision, mut starts, keyed) = {
                let state = lock(&self.state);
                let singletons: Vec<Arc<Singleton>> = state.singletons.values().cloned().collect();
                let keyed: Vec<Arc<KeyedBinding>> = state.keyed.values().cloned().collect();
                (state.readiness_revision, vec![state.transition.clone()], (singletons, keyed))
            };
            starts.extend(keyed.0.iter().filter_map(|binding| lock(&binding.state).starting.clone()));
            starts.extend(keyed.1.iter().map(|binding| binding.ready()));
            await_with_context(async { futures::future::try_join_all(starts).await.map(drop) }, context).await?;
            if self.is_disposed() {
                return Err(disposed_error());
            }
            if revision == lock(&self.state).readiness_revision {
                return Ok(());
            }
        }
    }

    async fn rebind(&self, bound: bool, context: &Context) -> ChordResult<()> {
        if self.is_disposed() {
            return Err(disposed_error());
        }
        self.bound.store(bound, Ordering::SeqCst);
        let (singletons, keyed) = {
            let mut state = lock(&self.state);
            state.readiness_revision += 1;
            let singletons: Vec<(String, Arc<Singleton>)> =
                state.singletons.iter().map(|(id, binding)| (id.clone(), binding.clone())).collect();
            (singletons, state.keyed.values().cloned().collect::<Vec<_>>())
        };
        let mut transitions: Vec<BoxFuture<ChordResult<()>>> = Vec::new();
        for (id, binding) in singletons {
            let (revision, subscription) = {
                let mut state = lock(&binding.state);
                state.revision += 1;
                (state.revision, state.subscription.take())
            };
            binding.facade.clear();
            let (weak, target, context) = (self.me.clone(), binding.clone(), context.clone());
            let task = start_shared(async move {
                if let Some(subscription) = subscription {
                    subscription.close(&context).await?;
                }
                match weak.upgrade() {
                    Some(inner) if bound => inner.start_singleton(id, target, revision).await,
                    _ => Ok(()),
                }
            });
            {
                let mut state = lock(&binding.state);
                if state.revision == revision {
                    state.starting = Some(task.clone());
                }
            }
            transitions.push(Box::pin(task));
        }
        for binding in keyed {
            let context = context.clone();
            transitions.push(Box::pin(start_shared(async move { binding.rebind(bound, &context).await })));
        }
        let completion = start_shared(settle_all(transitions, "Failed to rebind services"));
        {
            let mut state = lock(&self.state);
            state.transition = completion.clone();
            state.readiness_revision += 1;
        }
        completion.await
    }

    async fn dispose(&self, context: &Context) -> ChordResult<()> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let (singletons, keyed) = {
            let mut state = lock(&self.state);
            let singletons: Vec<Arc<Singleton>> = state.singletons.drain().map(|(_, binding)| binding).collect();
            (singletons, state.keyed.drain().map(|(_, binding)| binding).collect::<Vec<_>>())
        };
        let mut closes: Vec<BoxFuture<ChordResult<()>>> = Vec::new();
        for binding in singletons {
            let (starting, subscription) = {
                let mut state = lock(&binding.state);
                state.active = false;
                (state.starting.take(), state.subscription.take())
            };
            binding.facade.clear();
            if let Some(starting) = starting {
                closes.push(Box::pin(async move {
                    let _ = starting.await;
                    Ok(())
                }));
            }
            if let Some(subscription) = subscription {
                closes.push(subscription.close(context));
            }
        }
        for binding in keyed {
            let context = context.clone();
            closes.push(Box::pin(async move { binding.close(&context).await }));
        }
        settle_all(closes, "Failed to dispose services").await
    }
}

fn apply_singleton_update(
    facade: &ServiceFacade,
    service_id: &str,
    update: ServiceProviderUpdate,
    context: &Context,
) -> ChordResult<()> {
    match update {
        ServiceProviderUpdateOf::Reset { snapshot } => {
            validate_reset_snapshot(&snapshot, service_id, ServiceMode::Singleton)?;
            match snapshot.instances.first() {
                None => facade.clear(),
                Some(instance) => facade.install(instance, context)?,
            }
            Ok(())
        }
        ServiceProviderUpdateOf::Unavailable => {
            facade.clear();
            Ok(())
        }
        ServiceProviderUpdateOf::Replaced { snapshot } => {
            if snapshot.instance.is_some() {
                return Err(ChordError::error("Singleton replacement has an instance address"));
            }
            facade.install(&snapshot, context)
        }
        ServiceProviderUpdateOf::State { instance: None, member, sequence, ops } => {
            facade.update(&member, sequence, &ops, context)
        }
        _ => Ok(()),
    }
}

impl RemoteServiceBinding {
    /// Unbind or bind again in place; handles stay valid and resubscribe when bound.
    pub async fn rebind(&self, bound: bool, context: &Context) -> ChordResult<()> {
        self.0.rebind(bound, context).await
    }
}

#[async_trait]
impl RemoteServices for RemoteServiceBinding {
    fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceHandle> {
        self.0.use_service(service)
    }

    fn observe(&self, service: &ServiceRef, handler: ObserveHandler) -> ChordResult<Unsubscribe> {
        self.0.observe(service, handler)
    }

    async fn ready(&self, context: &Context) -> ChordResult<()> {
        self.0.ready(context).await
    }

    async fn dispose(&self, context: &Context) -> ChordResult<()> {
        self.0.dispose(context).await
    }
}
