//! One provider-side subscription: an atomic baseline plus a bounded FIFO of later updates.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};

use super::provider::ProviderInner;
use super::wire::{
    ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMemberSnapshotOf, ServiceProviderUpdate,
    ServiceProviderUpdateOf, ServiceSubscriptionSnapshot,
};
use crate::context::Context;
use crate::error::{ChordError, ChordResult};
use crate::sync::{BoxFuture, lock};

/// Receives provider updates for one subscription, in order.
pub type ServiceUpdateListener = Arc<dyn Fn(ServiceProviderUpdate, &Context) -> ChordResult<()> + Send + Sync>;

/// At most this many updates wait behind the running delivery; one more becomes a full reset.
pub(crate) const MAX_PENDING_UPDATES: usize = 100;

pub(crate) type StateKey = (Option<(String, u64)>, String);

pub(crate) fn state_key(instance: &Option<ServiceInstanceAddress>, member: &str) -> StateKey {
    (instance.as_ref().map(|address| (address.key.clone(), address.generation)), member.to_string())
}

#[derive(Default)]
struct Queue {
    buffer: VecDeque<(ServiceProviderUpdate, Context)>,
    snapshot_sequences: HashMap<StateKey, u64>,
    active: bool,
    draining: bool,
    terminated: bool,
    closed: bool,
}

pub(crate) struct ProviderSubscriber {
    listener: ServiceUpdateListener,
    queue: Mutex<Queue>,
}

pub(crate) fn record_snapshot_sequences(sequences: &mut HashMap<StateKey, u64>, instances: &[ServiceInstanceSnapshot]) {
    for instance in instances {
        for member in &instance.members {
            if let ServiceMemberSnapshotOf::State { name, sequence, .. } = member {
                sequences.insert(state_key(&instance.instance, name), *sequence);
            }
        }
    }
}

fn covered_by_snapshot(sequences: &mut HashMap<StateKey, u64>, update: &ServiceProviderUpdate) -> bool {
    match update {
        ServiceProviderUpdateOf::State { instance, member, sequence, .. } => {
            let key = state_key(instance, member);
            match sequences.get(&key) {
                None => false,
                Some(baseline) if sequence <= baseline => true,
                Some(_) => {
                    sequences.remove(&key);
                    false
                }
            }
        }
        ServiceProviderUpdateOf::Reset { snapshot } => {
            sequences.clear();
            record_snapshot_sequences(sequences, &snapshot.instances);
            false
        }
        ServiceProviderUpdateOf::Replaced { snapshot } => {
            sequences.clear();
            record_snapshot_sequences(sequences, std::slice::from_ref(snapshot));
            false
        }
        ServiceProviderUpdateOf::Spawned { instance } => {
            record_snapshot_sequences(sequences, std::slice::from_ref(instance));
            false
        }
        ServiceProviderUpdateOf::Unavailable => {
            sequences.clear();
            false
        }
        ServiceProviderUpdateOf::Closed { instance } => {
            sequences.retain(|(address, _), _| {
                address
                    .as_ref()
                    .is_none_or(|(key, generation)| *key != instance.key || *generation != instance.generation)
            });
            false
        }
    }
}

impl ProviderSubscriber {
    pub fn new(listener: ServiceUpdateListener, snapshot: &ServiceSubscriptionSnapshot) -> Arc<Self> {
        let mut queue = Queue::default();
        record_snapshot_sequences(&mut queue.snapshot_sequences, &snapshot.instances);
        Arc::new(Self { listener, queue: Mutex::new(queue) })
    }

    /// Queue one update; `reset` builds the overflow replacement snapshot.
    pub fn enqueue(
        &self,
        update: &ServiceProviderUpdate,
        context: &Context,
        reset: &dyn Fn() -> ServiceSubscriptionSnapshot,
    ) {
        let mut queue = lock(&self.queue);
        if queue.closed || covered_by_snapshot(&mut queue.snapshot_sequences, update) {
            return;
        }
        if queue.buffer.len() == MAX_PENDING_UPDATES {
            let snapshot = reset();
            queue.buffer.clear();
            queue.snapshot_sequences.clear();
            record_snapshot_sequences(&mut queue.snapshot_sequences, &snapshot.instances);
            queue.buffer.push_back((ServiceProviderUpdateOf::Reset { snapshot }, context.clone()));
        } else {
            queue.buffer.push_back((update.clone(), context.clone()));
        }
    }

    pub fn drain(&self) -> Vec<ChordError> {
        {
            let mut queue = lock(&self.queue);
            if !queue.active || queue.closed || queue.draining {
                return Vec::new();
            }
            queue.draining = true;
        }
        let mut errors = Vec::new();
        loop {
            let entry = {
                let mut queue = lock(&self.queue);
                if queue.closed { None } else { queue.buffer.pop_front() }
            };
            let Some((update, context)) = entry else { break };
            if let Err(error) = (self.listener)(update, &context) {
                errors.push(error);
            }
        }
        let mut queue = lock(&self.queue);
        queue.draining = false;
        if queue.terminated {
            queue.closed = true;
        }
        errors
    }

    pub fn activate(&self) -> ChordResult<()> {
        {
            let mut queue = lock(&self.queue);
            if queue.closed || queue.active {
                return Ok(());
            }
            queue.active = true;
        }
        ChordError::collect(self.drain(), "Failed to activate remote service subscription")
    }

    /// Close now, or after the running delivery when one is in progress.
    pub fn terminate(&self) {
        let mut queue = lock(&self.queue);
        if queue.active && !queue.draining {
            queue.closed = true;
            queue.buffer.clear();
        } else {
            queue.terminated = true;
        }
    }

    pub fn close(&self) -> bool {
        let mut queue = lock(&self.queue);
        if queue.closed {
            return false;
        }
        queue.closed = true;
        queue.buffer.clear();
        true
    }
}

/// A provider-side subscription handle: the atomic snapshot, activation, and close.
pub struct ProviderSubscription {
    pub(crate) snapshot: ServiceSubscriptionSnapshot,
    pub(crate) subscriber: Arc<ProviderSubscriber>,
    pub(crate) provider: Weak<ProviderInner>,
    pub(crate) service_id: String,
}

impl ProviderSubscription {
    /// The atomic baseline. Later updates buffer until activation.
    pub fn snapshot(&self) -> &ServiceSubscriptionSnapshot {
        &self.snapshot
    }

    /// Start delivery, draining buffered updates first.
    pub fn activate(&self) -> ChordResult<()> {
        self.subscriber.activate()
    }

    /// Stop delivery and discard buffered updates. Idempotent.
    pub fn close(&self) {
        if !self.subscriber.close() {
            return;
        }
        if let Some(provider) = self.provider.upgrade() {
            provider.remove_subscriber(&self.service_id, &self.subscriber);
        }
    }
}

impl super::transport::ServiceSubscription for ProviderSubscription {
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot {
        &self.snapshot
    }

    fn activate(&self) -> ChordResult<()> {
        ProviderSubscription::activate(self)
    }

    fn close(&self, _context: &Context) -> BoxFuture<ChordResult<()>> {
        ProviderSubscription::close(self);
        Box::pin(std::future::ready(Ok(())))
    }
}
