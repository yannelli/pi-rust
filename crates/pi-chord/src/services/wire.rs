//! Chord's transport-independent service wire types, generic over decoded [`Op`] or encoded [`WireOp`].

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::delta::{Op, WireOp, parse_op, parse_wire_op};
use crate::error::ChordResult;
use crate::json::{Json, JsonMap};

/// An operation vocabulary that can appear in snapshots and updates.
pub trait OpForm: Clone + PartialEq + fmt::Debug + Send + Sync + 'static {
    fn to_json(&self) -> Json;
    fn parse(value: &Json) -> ChordResult<Self>;
    fn is_replace(&self) -> bool;
}

impl OpForm for Op {
    fn to_json(&self) -> Json {
        Op::to_json(self)
    }
    fn parse(value: &Json) -> ChordResult<Self> {
        parse_op(value)
    }
    fn is_replace(&self) -> bool {
        Op::is_replace(self)
    }
}

impl OpForm for WireOp {
    fn to_json(&self) -> Json {
        WireOp::to_json(self)
    }
    fn parse(value: &Json) -> ChordResult<Self> {
        parse_wire_op(value)
    }
    fn is_replace(&self) -> bool {
        WireOp::is_replace(self)
    }
}

/// Whether a service has one provider or dynamic keyed instances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceMode {
    Singleton,
    Keyed,
}

impl ServiceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceMode::Singleton => "singleton",
            ServiceMode::Keyed => "keyed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "singleton" => Some(ServiceMode::Singleton),
            "keyed" => Some(ServiceMode::Keyed),
            _ => None,
        }
    }
}

impl fmt::Display for ServiceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One catalogue entry: `{ serviceId, mode }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceCatalogueEntry {
    pub service_id: String,
    pub mode: ServiceMode,
}

/// A keyed instance address: `{ key, generation }`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceInstanceAddress {
    pub key: String,
    pub generation: u64,
}

/// One member of an instance snapshot.
#[derive(Clone, Debug, PartialEq)]
pub enum ServiceMemberSnapshotOf<O> {
    Method { name: String },
    State { name: String, sequence: u64, ops: Vec<O> },
}

impl<O> ServiceMemberSnapshotOf<O> {
    pub fn name(&self) -> &str {
        match self {
            Self::Method { name } | Self::State { name, .. } => name,
        }
    }
}

/// One instance: its optional keyed address and member snapshots.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceInstanceSnapshotOf<O> {
    pub instance: Option<ServiceInstanceAddress>,
    pub members: Vec<ServiceMemberSnapshotOf<O>>,
}

/// The atomic baseline of one subscription.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceSubscriptionSnapshotOf<O> {
    pub service_id: String,
    pub mode: ServiceMode,
    pub instances: Vec<ServiceInstanceSnapshotOf<O>>,
}

/// One provider update after the baseline.
#[derive(Clone, Debug, PartialEq)]
pub enum ServiceProviderUpdateOf<O> {
    State {
        instance: Option<ServiceInstanceAddress>,
        member: String,
        sequence: u64,
        ops: Vec<O>,
    },
    /// Full subscription rebaseline after overflow; every state is a root replacement.
    Reset {
        snapshot: ServiceSubscriptionSnapshotOf<O>,
    },
    Unavailable,
    Replaced {
        snapshot: ServiceInstanceSnapshotOf<O>,
    },
    Spawned {
        instance: ServiceInstanceSnapshotOf<O>,
    },
    Closed {
        instance: ServiceInstanceAddress,
    },
}

pub type ServiceMemberSnapshot = ServiceMemberSnapshotOf<Op>;
pub type ServiceInstanceSnapshot = ServiceInstanceSnapshotOf<Op>;
pub type ServiceSubscriptionSnapshot = ServiceSubscriptionSnapshotOf<Op>;
pub type ServiceProviderUpdate = ServiceProviderUpdateOf<Op>;
pub type WireServiceMemberSnapshot = ServiceMemberSnapshotOf<WireOp>;
pub type WireServiceInstanceSnapshot = ServiceInstanceSnapshotOf<WireOp>;
pub type WireServiceSubscriptionSnapshot = ServiceSubscriptionSnapshotOf<WireOp>;
pub type WireServiceProviderUpdate = ServiceProviderUpdateOf<WireOp>;

/// One logical method call. `args` are borrowed immutable values.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceCall {
    pub service_id: String,
    pub instance: Option<ServiceInstanceAddress>,
    pub member: String,
    pub args: Vec<Json>,
}

pub(crate) fn object(entries: Vec<(&str, Json)>) -> Json {
    let mut map = JsonMap::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Json::object(map)
}

fn ops_json<O: OpForm>(ops: &[O]) -> Json {
    Json::array(ops.iter().map(OpForm::to_json).collect())
}

impl ServiceCatalogueEntry {
    pub fn to_json(&self) -> Json {
        object(vec![("serviceId", self.service_id.as_str().into()), ("mode", self.mode.as_str().into())])
    }
}

/// A catalogue as a JSON array.
pub fn catalogue_to_json(entries: &[ServiceCatalogueEntry]) -> Json {
    Json::array(entries.iter().map(ServiceCatalogueEntry::to_json).collect())
}

impl ServiceInstanceAddress {
    pub fn new(key: impl Into<String>, generation: u64) -> Self {
        Self { key: key.into(), generation }
    }

    pub fn to_json(&self) -> Json {
        object(vec![("key", self.key.as_str().into()), ("generation", self.generation.into())])
    }
}

impl<O: OpForm> ServiceMemberSnapshotOf<O> {
    pub fn to_json(&self) -> Json {
        match self {
            Self::Method { name } => object(vec![("name", name.as_str().into()), ("kind", "method".into())]),
            Self::State { name, sequence, ops } => object(vec![
                ("name", name.as_str().into()),
                ("kind", "state".into()),
                ("sequence", (*sequence).into()),
                ("ops", ops_json(ops)),
            ]),
        }
    }
}

impl<O: OpForm> ServiceInstanceSnapshotOf<O> {
    pub fn to_json(&self) -> Json {
        let mut entries = Vec::new();
        if let Some(address) = &self.instance {
            entries.push(("instance", address.to_json()));
        }
        entries.push(("members", Json::array(self.members.iter().map(|member| member.to_json()).collect())));
        object(entries)
    }
}

impl<O: OpForm> ServiceSubscriptionSnapshotOf<O> {
    pub fn to_json(&self) -> Json {
        object(vec![
            ("serviceId", self.service_id.as_str().into()),
            ("mode", self.mode.as_str().into()),
            ("instances", Json::array(self.instances.iter().map(|instance| instance.to_json()).collect())),
        ])
    }
}

impl<O: OpForm> ServiceProviderUpdateOf<O> {
    /// The `type` tag.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::State { .. } => "state",
            Self::Reset { .. } => "reset",
            Self::Unavailable => "unavailable",
            Self::Replaced { .. } => "replaced",
            Self::Spawned { .. } => "spawned",
            Self::Closed { .. } => "closed",
        }
    }

    pub fn to_json(&self) -> Json {
        let mut entries = vec![("type", Json::from(self.type_name()))];
        match self {
            Self::State { instance, member, sequence, ops } => {
                if let Some(address) = instance {
                    entries.push(("instance", address.to_json()));
                }
                entries.push(("member", member.as_str().into()));
                entries.push(("sequence", (*sequence).into()));
                entries.push(("ops", ops_json(ops)));
            }
            Self::Reset { snapshot } => entries.push(("snapshot", snapshot.to_json())),
            Self::Unavailable => {}
            Self::Replaced { snapshot } => entries.push(("snapshot", snapshot.to_json())),
            Self::Spawned { instance } => entries.push(("instance", instance.to_json())),
            Self::Closed { instance } => entries.push(("instance", instance.to_json())),
        }
        object(entries)
    }
}

impl ServiceCall {
    pub fn to_json(&self) -> Json {
        let mut entries = vec![("serviceId", Json::from(self.service_id.as_str()))];
        if let Some(address) = &self.instance {
            entries.push(("instance", address.to_json()));
        }
        entries.push(("member", self.member.as_str().into()));
        entries.push(("args", Json::array(self.args.clone())));
        object(entries)
    }
}

macro_rules! json_serde {
    ($ty:ty, $parse:path) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.to_json().serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = Json::deserialize(deserializer)?;
                $parse(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

json_serde!(ServiceCall, super::wire_parse::parse_service_call);
json_serde!(ServiceInstanceAddress, super::wire_parse::parse_address);
json_serde!(ServiceSubscriptionSnapshot, super::wire_parse::parse_service_subscription_snapshot);
json_serde!(WireServiceSubscriptionSnapshot, super::wire_parse::parse_wire_service_subscription_snapshot);
json_serde!(ServiceProviderUpdate, super::wire_parse::parse_service_provider_update);
json_serde!(WireServiceProviderUpdate, super::wire_parse::parse_wire_service_provider_update);

impl Serialize for ServiceCatalogueEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}
