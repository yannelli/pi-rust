//! The consumer side of one keyed service: one subscription feeding an instance directory.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::facade::ServiceFacade;
use crate::context::{BACKGROUND_CONTEXT, Context};
use crate::error::{ChordError, ChordResult, RemoteServiceErrorCode as Code};
use crate::services::handle::{AccessCheck, ServiceHandle, ServiceSlot, SlotTarget};
use crate::services::instances::{InstanceDirectory, InstanceEntry, InstanceHandler, InstanceService};
use crate::services::remote::ObserveHandler;
use crate::services::transport::{RemoteServiceTransport, ServiceSubscription};
use crate::services::validate::validate_reset_snapshot;
use crate::services::wire::{ServiceInstanceSnapshot, ServiceMode, ServiceProviderUpdate, ServiceProviderUpdateOf};
use crate::state::{ErrorReporter, service_delivery_context};
use crate::sync::{SharedTask, Unsubscribe, lock, ready_task, start_shared, yield_now};

#[derive(Default)]
struct State {
    subscription: Option<Arc<dyn ServiceSubscription>>,
    starting: Option<SharedTask>,
    start_requested: bool,
    closed: bool,
    bound: bool,
    revision: u64,
}

pub(crate) type OnEmpty = Box<dyn Fn(&Arc<KeyedBinding>) + Send + Sync>;

pub(crate) struct KeyedBinding {
    service_id: String,
    transport: Arc<dyn RemoteServiceTransport>,
    report_error: ErrorReporter,
    assert_access: AccessCheck,
    on_empty: OnEmpty,
    instances: Arc<InstanceDirectory>,
    state: Mutex<State>,
    me: Weak<KeyedBinding>,
}

impl KeyedBinding {
    pub fn new(
        service_id: &str,
        transport: Arc<dyn RemoteServiceTransport>,
        report_error: ErrorReporter,
        assert_access: AccessCheck,
        on_empty: OnEmpty,
        bound: bool,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            service_id: service_id.to_string(),
            transport,
            instances: InstanceDirectory::new(false, report_error.clone()),
            report_error,
            assert_access,
            on_empty,
            state: Mutex::new(State { bound, ..State::default() }),
            me: me.clone(),
        })
    }

    pub fn observe(self: &Arc<Self>, handler: ObserveHandler) -> ChordResult<Unsubscribe> {
        if lock(&self.state).closed {
            return Err(ChordError::error("Remote keyed service binding is closed"));
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let (service_id, assert_access, flag) = (self.service_id.clone(), self.assert_access.clone(), stopped.clone());
        let wrapped: InstanceHandler = Arc::new(move |service, context: Context| {
            let InstanceService::Remote(target) = service else {
                return Box::pin(std::future::ready(Err(ChordError::type_error("Keyed instance is not remote"))));
            };
            let slot = ServiceSlot::new(&service_id);
            slot.bind(SlotTarget::Remote(target));
            let (assert_access, flag, task_context, id) =
                (assert_access.clone(), flag.clone(), context.clone(), service_id.clone());
            let view: ServiceHandle = slot.remote_view(Arc::new(move || {
                assert_access()?;
                if flag.load(Ordering::SeqCst) || task_context.is_cancelled() {
                    let message = format!("Remote service {id} observation is closed");
                    return Err(ChordError::remote(Code::ServiceStaleInstance, message));
                }
                Ok(())
            }));
            handler(view, context)
        });
        let stop = self.instances.observe(wrapped)?;
        // Tasks start eagerly and may take the state lock, so they are created outside it.
        let revision = {
            let mut state = lock(&self.state);
            let start = state.bound && state.starting.is_none() && !state.start_requested;
            state.start_requested |= start;
            start.then_some(state.revision)
        };
        if let Some(revision) = revision {
            let task = self.start_reporting(revision);
            let mut state = lock(&self.state);
            state.start_requested = false;
            if state.revision == revision {
                state.starting = Some(task);
            }
        }
        let weak = self.me.clone();
        Ok(Unsubscribe::new(move || {
            if stopped.swap(true, Ordering::SeqCst) {
                return;
            }
            stop.unsubscribe();
            if let Some(binding) = weak.upgrade()
                && binding.instances.observer_count() == 0
            {
                (binding.on_empty)(&binding);
            }
        }))
    }

    fn start_reporting(&self, revision: u64) -> SharedTask {
        let weak = self.me.clone();
        start_shared(async move {
            let Some(binding) = weak.upgrade() else { return Ok(()) };
            let result = binding.clone().start(revision).await;
            if let Err(error) = &result {
                let current = {
                    let state = lock(&binding.state);
                    !state.closed && state.revision == revision && state.bound
                };
                if current {
                    (binding.report_error)(error.clone());
                }
            }
            result
        })
    }

    pub async fn rebind(self: &Arc<Self>, bound: bool, context: &Context) -> ChordResult<()> {
        let revision = {
            let mut state = lock(&self.state);
            if state.closed {
                return Ok(());
            }
            state.bound = bound;
            state.revision += 1;
            state.revision
        };
        self.reset(context, false).await?;
        {
            let state = lock(&self.state);
            if state.closed || state.revision != revision || state.bound != bound {
                return Ok(());
            }
        }
        if !bound || self.instances.observer_count() == 0 {
            return Ok(());
        }
        let weak = self.me.clone();
        let task = start_shared(async move {
            match weak.upgrade() {
                Some(binding) => binding.start(revision).await,
                None => Ok(()),
            }
        });
        lock(&self.state).starting = Some(task.clone());
        task.await
    }

    pub fn ready(&self) -> SharedTask {
        lock(&self.state).starting.clone().unwrap_or_else(|| ready_task(Ok(())))
    }

    pub async fn close(self: &Arc<Self>, context: &Context) -> ChordResult<()> {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return Ok(());
            }
            state.closed = true;
            state.revision += 1;
        }
        let result = self.reset(context, true).await;
        self.instances.dispose();
        result
    }

    async fn reset(&self, context: &Context, wait_for_starting: bool) -> ChordResult<()> {
        self.instances.reset();
        let (starting, subscription) = {
            let mut state = lock(&self.state);
            (state.starting.take(), state.subscription.take())
        };
        if wait_for_starting && let Some(starting) = starting {
            let _ = starting.await;
        }
        match subscription {
            Some(subscription) => subscription.close(context).await,
            None => Ok(()),
        }
    }

    async fn start(self: Arc<Self>, revision: u64) -> ChordResult<()> {
        let weak = self.me.clone();
        let listener = Arc::new(move |update: ServiceProviderUpdate, context: &Context| {
            if let Some(binding) = weak.upgrade()
                && lock(&binding.state).revision == revision
            {
                binding.update(update, context);
            }
            Ok(())
        });
        let subscription =
            self.transport.subscribe(&self.service_id, ServiceMode::Keyed, listener, &BACKGROUND_CONTEXT).await?;
        // Installation continues on a later turn, as after upstream's await on the subscription.
        yield_now().await;
        let current = {
            let mut state = lock(&self.state);
            let current = !state.closed && state.bound && state.revision == revision;
            if current {
                state.subscription = Some(subscription.clone());
            }
            current
        };
        if !current {
            return subscription.close(&BACKGROUND_CONTEXT).await;
        }
        let snapshot = subscription.snapshot();
        if snapshot.mode != ServiceMode::Keyed || snapshot.service_id != self.service_id {
            return Err(ChordError::error(format!(
                "Remote service {} returned the wrong keyed snapshot",
                self.service_id
            )));
        }
        for instance in &snapshot.instances {
            self.spawn_instance(instance, &service_delivery_context())?;
        }
        subscription.activate()?;
        self.instances.ready()
    }

    fn update(&self, update: ServiceProviderUpdate, context: &Context) {
        if lock(&self.state).closed {
            return;
        }
        if let Err(error) = self.apply_update(update, context) {
            (self.report_error)(error);
        }
    }

    fn apply_update(&self, update: ServiceProviderUpdate, context: &Context) -> ChordResult<()> {
        match update {
            ServiceProviderUpdateOf::Reset { snapshot } => {
                validate_reset_snapshot(&snapshot, &self.service_id, ServiceMode::Keyed)?;
                let generation_of = |key: &str| {
                    snapshot
                        .instances
                        .iter()
                        .find(|instance| instance.instance.as_ref().is_some_and(|address| address.key == key))
                        .and_then(|instance| instance.instance.as_ref().map(|address| address.generation))
                };
                for instance in self.instances.values() {
                    if generation_of(&instance.key) != Some(instance.generation) {
                        self.instances.remove(&instance);
                    }
                }
                for instance in &snapshot.instances {
                    let key = instance.instance.as_ref().map(|address| address.key.clone()).unwrap_or_default();
                    match self.instances.get(&key) {
                        None => self.spawn_instance(instance, context)?,
                        Some(existing) => {
                            if let Some(facade) = &existing.facade {
                                facade.install(instance, context)?;
                            }
                        }
                    }
                }
                Ok(())
            }
            ServiceProviderUpdateOf::Unavailable | ServiceProviderUpdateOf::Replaced { .. } => {
                Err(ChordError::error("Keyed service received a singleton lifecycle update"))
            }
            ServiceProviderUpdateOf::Spawned { instance } => self.spawn_instance(&instance, context),
            ServiceProviderUpdateOf::Closed { instance } => {
                if let Some(existing) = self.instances.get(&instance.key)
                    && existing.generation == instance.generation
                {
                    self.instances.remove(&existing);
                }
                Ok(())
            }
            ServiceProviderUpdateOf::State { instance, member, sequence, ops } => {
                let address =
                    instance.ok_or_else(|| ChordError::error("Keyed state update has no instance address"))?;
                let Some(existing) = self.instances.get(&address.key) else { return Ok(()) };
                if existing.generation != address.generation {
                    return Ok(());
                }
                match &existing.facade {
                    Some(facade) => facade.update(&member, sequence, &ops, context),
                    None => Ok(()),
                }
            }
        }
    }

    fn spawn_instance(&self, snapshot: &ServiceInstanceSnapshot, context: &Context) -> ChordResult<()> {
        let address = snapshot
            .instance
            .clone()
            .ok_or_else(|| ChordError::error("Keyed service instance snapshot has no address"))?;
        let active = Arc::new(AtomicBool::new(true));
        let (flag, weak) = (active.clone(), self.me.clone());
        let facade = ServiceFacade::new(
            &self.service_id,
            Some(address.clone()),
            self.transport.clone(),
            Arc::new(move || {
                flag.load(Ordering::SeqCst) && weak.upgrade().is_some_and(|binding| !lock(&binding.state).closed)
            }),
            self.assert_access.clone(),
            self.report_error.clone(),
        );
        facade.install(snapshot, context)?;
        let cleared = facade.clone();
        self.instances.replace(Arc::new(InstanceEntry {
            key: address.key,
            generation: address.generation,
            service: InstanceService::Remote(ServiceHandle(facade.clone())),
            facade: Some(facade),
            deactivate: Box::new(move || {
                active.store(false, Ordering::SeqCst);
                cleared.clear();
            }),
        }))
    }
}
