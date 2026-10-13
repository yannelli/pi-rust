//! Per-subscription operation codecs: one independent path dictionary for every replicated state.

use std::collections::HashMap;

use super::wire::{
    ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceInstanceSnapshotOf, ServiceMemberSnapshotOf,
    ServiceProviderUpdate, ServiceProviderUpdateOf, ServiceSubscriptionSnapshot, ServiceSubscriptionSnapshotOf,
    WireServiceInstanceSnapshot, WireServiceProviderUpdate, WireServiceSubscriptionSnapshot,
};
use crate::delta::{Decoder, Encoder};
use crate::error::{ChordError, ChordResult};

type StateKey = (Option<(String, u64)>, String);

fn state_key(instance: &Option<ServiceInstanceAddress>, member: &str) -> StateKey {
    (instance.as_ref().map(|address| (address.key.clone(), address.generation)), member.to_string())
}

fn describe(instance: &Option<ServiceInstanceAddress>, member: &str) -> String {
    match instance {
        Some(address) => format!("{}@{}.{member}", address.key, address.generation),
        None => member.to_string(),
    }
}

struct Registry<C> {
    entries: HashMap<StateKey, C>,
}

impl<C: Default> Registry<C> {
    fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    fn reset(&mut self) {
        self.entries.clear();
    }

    fn add(&mut self, instance: &Option<ServiceInstanceAddress>, member: &str) -> ChordResult<&mut C> {
        let key = state_key(instance, member);
        if self.entries.contains_key(&key) {
            return Err(ChordError::error(format!("Duplicate service state {}", describe(instance, member))));
        }
        Ok(self.entries.entry(key).or_default())
    }

    fn get(&mut self, instance: &Option<ServiceInstanceAddress>, member: &str) -> ChordResult<&mut C> {
        self.entries
            .get_mut(&state_key(instance, member))
            .ok_or_else(|| ChordError::error(format!("Unknown service state {}", describe(instance, member))))
    }

    fn remove_instance(&mut self, address: &ServiceInstanceAddress) {
        self.entries.retain(|(instance, _), _| {
            instance.as_ref().is_none_or(|(key, generation)| *key != address.key || *generation != address.generation)
        });
    }
}

/// Stateful operation encoders for every replicated state in one service subscription.
pub struct ServiceStateEncoder {
    codecs: Registry<Encoder>,
}

/// Stateful operation decoders for every replicated state in one service subscription.
pub struct ServiceStateDecoder {
    codecs: Registry<Decoder>,
}

pub fn create_service_state_encoder() -> ServiceStateEncoder {
    ServiceStateEncoder { codecs: Registry::new() }
}

pub fn create_service_state_decoder() -> ServiceStateDecoder {
    ServiceStateDecoder { codecs: Registry::new() }
}

impl ServiceStateEncoder {
    fn encode_instance(&mut self, instance: &ServiceInstanceSnapshot) -> ChordResult<WireServiceInstanceSnapshot> {
        let mut members = Vec::with_capacity(instance.members.len());
        for member in &instance.members {
            members.push(match member {
                ServiceMemberSnapshotOf::Method { name } => ServiceMemberSnapshotOf::Method { name: name.clone() },
                ServiceMemberSnapshotOf::State { name, sequence, ops } => ServiceMemberSnapshotOf::State {
                    name: name.clone(),
                    sequence: *sequence,
                    ops: self.codecs.add(&instance.instance, name)?.encode(ops),
                },
            });
        }
        Ok(ServiceInstanceSnapshotOf { instance: instance.instance.clone(), members })
    }

    fn encode_instances(
        &mut self,
        snapshot: &ServiceSubscriptionSnapshot,
    ) -> ChordResult<WireServiceSubscriptionSnapshot> {
        let instances = snapshot
            .instances
            .iter()
            .map(|instance| self.encode_instance(instance))
            .collect::<ChordResult<Vec<_>>>()?;
        Ok(ServiceSubscriptionSnapshotOf { service_id: snapshot.service_id.clone(), mode: snapshot.mode, instances })
    }

    pub fn encode_snapshot(
        &mut self,
        snapshot: &ServiceSubscriptionSnapshot,
    ) -> ChordResult<WireServiceSubscriptionSnapshot> {
        self.codecs.reset();
        self.encode_instances(snapshot)
    }

    pub fn encode_update(&mut self, update: &ServiceProviderUpdate) -> ChordResult<WireServiceProviderUpdate> {
        Ok(match update {
            ServiceProviderUpdateOf::Reset { snapshot } => {
                self.codecs.reset();
                ServiceProviderUpdateOf::Reset { snapshot: self.encode_instances(snapshot)? }
            }
            ServiceProviderUpdateOf::State { instance, member, sequence, ops } => ServiceProviderUpdateOf::State {
                instance: instance.clone(),
                member: member.clone(),
                sequence: *sequence,
                ops: self.codecs.get(instance, member)?.encode(ops),
            },
            ServiceProviderUpdateOf::Replaced { snapshot } => {
                self.codecs.reset();
                ServiceProviderUpdateOf::Replaced { snapshot: self.encode_instance(snapshot)? }
            }
            ServiceProviderUpdateOf::Spawned { instance } => {
                ServiceProviderUpdateOf::Spawned { instance: self.encode_instance(instance)? }
            }
            ServiceProviderUpdateOf::Unavailable => {
                self.codecs.reset();
                ServiceProviderUpdateOf::Unavailable
            }
            ServiceProviderUpdateOf::Closed { instance } => {
                self.codecs.remove_instance(instance);
                ServiceProviderUpdateOf::Closed { instance: instance.clone() }
            }
        })
    }
}

impl ServiceStateDecoder {
    fn decode_instance(&mut self, instance: &WireServiceInstanceSnapshot) -> ChordResult<ServiceInstanceSnapshot> {
        let mut members = Vec::with_capacity(instance.members.len());
        for member in &instance.members {
            members.push(match member {
                ServiceMemberSnapshotOf::Method { name } => ServiceMemberSnapshotOf::Method { name: name.clone() },
                ServiceMemberSnapshotOf::State { name, sequence, ops } => ServiceMemberSnapshotOf::State {
                    name: name.clone(),
                    sequence: *sequence,
                    ops: self.codecs.add(&instance.instance, name)?.decode(ops)?,
                },
            });
        }
        Ok(ServiceInstanceSnapshotOf { instance: instance.instance.clone(), members })
    }

    fn decode_instances(
        &mut self,
        snapshot: &WireServiceSubscriptionSnapshot,
    ) -> ChordResult<ServiceSubscriptionSnapshot> {
        let instances = snapshot
            .instances
            .iter()
            .map(|instance| self.decode_instance(instance))
            .collect::<ChordResult<Vec<_>>>()?;
        Ok(ServiceSubscriptionSnapshotOf { service_id: snapshot.service_id.clone(), mode: snapshot.mode, instances })
    }

    pub fn decode_snapshot(
        &mut self,
        snapshot: &WireServiceSubscriptionSnapshot,
    ) -> ChordResult<ServiceSubscriptionSnapshot> {
        self.codecs.reset();
        self.decode_instances(snapshot)
    }

    pub fn decode_update(&mut self, update: &WireServiceProviderUpdate) -> ChordResult<ServiceProviderUpdate> {
        Ok(match update {
            ServiceProviderUpdateOf::Reset { snapshot } => {
                self.codecs.reset();
                ServiceProviderUpdateOf::Reset { snapshot: self.decode_instances(snapshot)? }
            }
            ServiceProviderUpdateOf::State { instance, member, sequence, ops } => ServiceProviderUpdateOf::State {
                instance: instance.clone(),
                member: member.clone(),
                sequence: *sequence,
                ops: self.codecs.get(instance, member)?.decode(ops)?,
            },
            ServiceProviderUpdateOf::Replaced { snapshot } => {
                self.codecs.reset();
                ServiceProviderUpdateOf::Replaced { snapshot: self.decode_instance(snapshot)? }
            }
            ServiceProviderUpdateOf::Spawned { instance } => {
                ServiceProviderUpdateOf::Spawned { instance: self.decode_instance(instance)? }
            }
            ServiceProviderUpdateOf::Unavailable => {
                self.codecs.reset();
                ServiceProviderUpdateOf::Unavailable
            }
            ServiceProviderUpdateOf::Closed { instance } => {
                self.codecs.remove_instance(instance);
                ServiceProviderUpdateOf::Closed { instance: instance.clone() }
            }
        })
    }
}
