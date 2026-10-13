//! Consumer facades: one per remote instance, with lazily created member slots.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::context::Context;
use crate::delta::Op;
use crate::error::{ChordError, ChordResult, RemoteServiceErrorCode as Code};
use crate::json::Json;
use crate::services::handle::{AccessCheck, ServiceTarget, StateHandle, StateTarget};
use crate::services::token::MemberKind;
use crate::services::transport::RemoteServiceTransport;
use crate::services::wire::{ServiceCall, ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMemberSnapshotOf};
use crate::state::{ErrorReporter, ReplicatedStateReplica, StateListener};
use crate::sync::{BoxFuture, Unsubscribe, lock};

pub(crate) type IsActive = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Default)]
struct Kinds {
    actual: Option<MemberKind>,
    expected: Option<MemberKind>,
}

/// One member of a remote facade: callable as a method or readable as replicated state.
pub(crate) struct MemberSlot {
    service_id: Arc<str>,
    member: String,
    address: Option<ServiceInstanceAddress>,
    transport: Arc<dyn RemoteServiceTransport>,
    state: Arc<ReplicatedStateReplica>,
    is_active: IsActive,
    assert_access: AccessCheck,
    kinds: Mutex<Kinds>,
}

impl MemberSlot {
    fn set_description(&self, kind: MemberKind) -> ChordResult<()> {
        let mut kinds = lock(&self.kinds);
        if kinds.actual.is_some_and(|actual| actual != kind) {
            return Err(ChordError::error(format!(
                "Remote service member {}.{} changed kind",
                self.service_id, self.member
            )));
        }
        kinds.actual = Some(kind);
        if let Some(expected) = kinds.expected.filter(|expected| *expected != kind) {
            return Err(ChordError::remote(
                Code::ServiceMemberMismatch,
                format!(
                    "Remote service member {}.{} is {}, not {}",
                    self.service_id,
                    self.member,
                    kind.as_str(),
                    expected.as_str()
                ),
            ));
        }
        Ok(())
    }

    fn expect(&self, kind: MemberKind) -> ChordResult<()> {
        let mut kinds = lock(&self.kinds);
        if kinds.expected.is_some_and(|expected| expected != kind) {
            return Err(ChordError::remote(
                Code::ServiceMemberMismatch,
                format!("Remote service member {}.{} was used as two different kinds", self.service_id, self.member),
            ));
        }
        kinds.expected = Some(kind);
        if let Some(actual) = kinds.actual.filter(|actual| *actual != kind) {
            return Err(ChordError::remote(
                Code::ServiceMemberMismatch,
                format!(
                    "Remote service member {}.{} is {}, not {}",
                    self.service_id,
                    self.member,
                    actual.as_str(),
                    kind.as_str()
                ),
            ));
        }
        Ok(())
    }

    fn hydrate(&self, sequence: u64, ops: &[Op], context: &Context) -> ChordResult<()> {
        self.set_description(MemberKind::State)?;
        self.state.hydrate(sequence, ops, context)
    }

    fn update(&self, sequence: u64, ops: &[Op], context: &Context) -> ChordResult<()> {
        self.set_description(MemberKind::State)?;
        self.state.update(sequence, ops, context)
    }

    fn call(&self, args: Vec<Json>, context: &Context) -> BoxFuture<ChordResult<Option<Json>>> {
        let checked = (self.assert_access)().and_then(|()| self.expect(MemberKind::Method)).and_then(|()| {
            if (self.is_active)() {
                Ok(())
            } else {
                let message = format!("Remote service {} binding is closed", self.service_id);
                Err(ChordError::remote(Code::ServiceStaleInstance, message))
            }
        });
        if let Err(error) = checked {
            return Box::pin(std::future::ready(Err(error)));
        }
        let call = ServiceCall {
            service_id: self.service_id.to_string(),
            instance: self.address.clone(),
            member: self.member.clone(),
            args,
        };
        let (transport, context) = (self.transport.clone(), context.clone());
        Box::pin(async move { transport.invoke(call, &context).await })
    }
}

impl StateTarget for MemberSlot {
    fn value(&self) -> ChordResult<Option<Json>> {
        (self.assert_access)()?;
        self.expect(MemberKind::State)?;
        Ok(self.state.value())
    }

    fn subscribe(&self, listener: StateListener) -> ChordResult<Unsubscribe> {
        (self.assert_access)()?;
        self.expect(MemberKind::State)?;
        Ok(self.state.subscribe(listener))
    }
}

/// The stable facade of one remote singleton or keyed instance.
pub(crate) struct ServiceFacade {
    service_id: Arc<str>,
    address: Option<ServiceInstanceAddress>,
    transport: Arc<dyn RemoteServiceTransport>,
    slots: Mutex<HashMap<String, Arc<MemberSlot>>>,
    descriptions: Mutex<HashMap<String, MemberKind>>,
    is_active: IsActive,
    assert_access: AccessCheck,
    report_error: ErrorReporter,
}

impl ServiceFacade {
    pub fn new(
        service_id: &str,
        address: Option<ServiceInstanceAddress>,
        transport: Arc<dyn RemoteServiceTransport>,
        is_active: IsActive,
        assert_access: AccessCheck,
        report_error: ErrorReporter,
    ) -> Arc<Self> {
        Arc::new(Self {
            service_id: service_id.into(),
            address,
            transport,
            slots: Mutex::new(HashMap::new()),
            descriptions: Mutex::new(HashMap::new()),
            is_active,
            assert_access,
            report_error,
        })
    }

    fn slot(&self, member: &str) -> ChordResult<Arc<MemberSlot>> {
        let created = {
            let mut slots = lock(&self.slots);
            if let Some(slot) = slots.get(member) {
                return Ok(slot.clone());
            }
            let slot = Arc::new(MemberSlot {
                service_id: self.service_id.clone(),
                member: member.to_string(),
                address: self.address.clone(),
                transport: self.transport.clone(),
                state: ReplicatedStateReplica::new(self.report_error.clone()),
                is_active: self.is_active.clone(),
                assert_access: self.assert_access.clone(),
                kinds: Mutex::new(Kinds::default()),
            });
            slots.insert(member.to_string(), slot.clone());
            slot
        };
        if let Some(kind) = lock(&self.descriptions).get(member).copied() {
            created.set_description(kind)?;
        }
        Ok(created)
    }

    /// Install a complete instance snapshot: describe members and hydrate every state.
    pub fn install(&self, snapshot: &ServiceInstanceSnapshot, context: &Context) -> ChordResult<()> {
        if snapshot.instance != self.address {
            return Err(ChordError::error("Remote service snapshot has the wrong address"));
        }
        let mut members: HashMap<&str, &ServiceMemberSnapshotOf<Op>> = HashMap::new();
        for member in &snapshot.members {
            if member.name().is_empty() || members.insert(member.name(), member).is_some() {
                return Err(ChordError::error("Remote service has invalid member descriptions"));
            }
        }
        let existing: Vec<String> = lock(&self.slots).keys().cloned().collect();
        if let Some(missing) = existing.iter().find(|name| !members.contains_key(name.as_str())) {
            return Err(ChordError::remote(
                Code::ServiceMemberNotFound,
                format!("Unknown remote service member {}.{missing}", self.service_id),
            ));
        }
        {
            let mut descriptions = lock(&self.descriptions);
            descriptions.clear();
            for member in &snapshot.members {
                let kind = match member {
                    ServiceMemberSnapshotOf::Method { .. } => MemberKind::Method,
                    ServiceMemberSnapshotOf::State { .. } => MemberKind::State,
                };
                descriptions.insert(member.name().to_string(), kind);
            }
        }
        for member in &snapshot.members {
            match member {
                ServiceMemberSnapshotOf::State { name, sequence, ops } => {
                    self.slot(name)?.hydrate(*sequence, ops, context)?;
                }
                ServiceMemberSnapshotOf::Method { name } => {
                    let slot = lock(&self.slots).get(name).cloned();
                    if let Some(slot) = slot {
                        slot.set_description(MemberKind::Method)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn update(&self, member: &str, sequence: u64, ops: &[Op], context: &Context) -> ChordResult<()> {
        if lock(&self.descriptions).get(member) != Some(&MemberKind::State) {
            return Err(ChordError::error(format!(
                "Remote service update targets non-state member {}.{member}",
                self.service_id
            )));
        }
        self.slot(member)?.update(sequence, ops, context)
    }

    pub fn clear(&self) {
        let slots: Vec<Arc<MemberSlot>> = lock(&self.slots).values().cloned().collect();
        for slot in slots {
            slot.state.clear();
        }
    }
}

impl ServiceTarget for ServiceFacade {
    fn call(&self, member: &str, args: Vec<Json>, context: &Context) -> BoxFuture<ChordResult<Option<Json>>> {
        match self.slot(member) {
            Ok(slot) => slot.call(args, context),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    fn state(&self, member: &str) -> ChordResult<StateHandle> {
        Ok(StateHandle(self.slot(member)?))
    }
}
