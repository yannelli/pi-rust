//! Strict parsers for the service wire grammar and the `$chord.service` control calls.

use super::wire::{
    OpForm, ServiceCall, ServiceCatalogueEntry, ServiceInstanceAddress, ServiceInstanceSnapshotOf,
    ServiceMemberSnapshotOf, ServiceMode, ServiceProviderUpdate, ServiceProviderUpdateOf, ServiceSubscriptionSnapshot,
    ServiceSubscriptionSnapshotOf, WireServiceProviderUpdate, WireServiceSubscriptionSnapshot,
};
use crate::delta::{Op, WireOp};
use crate::error::{ChordError, ChordResult};
use crate::json::{Json, JsonMap};

pub(crate) const SERVICE_CONTROL_ID: &str = "$chord.service";
const SERVICE_CATALOGUE_MEMBER: &str = "catalogue";
const SERVICE_SUBSCRIBE_MEMBER: &str = "subscribe";
const SERVICE_UNSUBSCRIBE_MEMBER: &str = "unsubscribe";

/// A decoded `$chord.service` control call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceControlCall {
    Catalogue,
    Subscribe { subscription_id: String, service_id: String, mode: ServiceMode },
    Unsubscribe { subscription_id: String },
}

pub fn create_service_catalogue_call() -> ServiceCall {
    ServiceCall {
        service_id: SERVICE_CONTROL_ID.into(),
        instance: None,
        member: SERVICE_CATALOGUE_MEMBER.into(),
        args: Vec::new(),
    }
}

pub fn create_service_subscribe_call(subscription_id: &str, service_id: &str, mode: ServiceMode) -> ServiceCall {
    ServiceCall {
        service_id: SERVICE_CONTROL_ID.into(),
        instance: None,
        member: SERVICE_SUBSCRIBE_MEMBER.into(),
        args: vec![subscription_id.into(), service_id.into(), mode.as_str().into()],
    }
}

pub fn create_service_unsubscribe_call(subscription_id: &str) -> ServiceCall {
    ServiceCall {
        service_id: SERVICE_CONTROL_ID.into(),
        instance: None,
        member: SERVICE_UNSUBSCRIBE_MEMBER.into(),
        args: vec![subscription_id.into()],
    }
}

fn id(value: &Json) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

/// Decode a control call, or `None` for an ordinary service call.
pub fn decode_service_control_call(call: &ServiceCall) -> Option<ServiceControlCall> {
    if call.service_id != SERVICE_CONTROL_ID || call.instance.is_some() {
        return None;
    }
    let args = &call.args;
    match call.member.as_str() {
        SERVICE_CATALOGUE_MEMBER if args.is_empty() => Some(ServiceControlCall::Catalogue),
        SERVICE_SUBSCRIBE_MEMBER if args.len() == 3 => Some(ServiceControlCall::Subscribe {
            subscription_id: id(&args[0])?.to_string(),
            service_id: id(&args[1])?.to_string(),
            mode: ServiceMode::parse(args[2].as_str()?)?,
        }),
        SERVICE_UNSUBSCRIBE_MEMBER if args.len() == 1 => {
            Some(ServiceControlCall::Unsubscribe { subscription_id: id(&args[0])?.to_string() })
        }
        _ => None,
    }
}

fn invalid(description: &str) -> ChordError {
    ChordError::type_error(format!("Invalid {description}"))
}

fn record<'a>(value: &'a Json, description: &str) -> ChordResult<&'a JsonMap> {
    value.as_object().ok_or_else(|| invalid(description))
}

fn assert_keys(value: &JsonMap, required: &[&str], optional: &[&str], description: &str) -> ChordResult<()> {
    let missing = required.iter().any(|key| !value.contains_key(key));
    let extra = value.keys().any(|key| !required.contains(&&**key) && !optional.contains(&&**key));
    if missing || extra { Err(invalid(description)) } else { Ok(()) }
}

fn integer(value: Option<&Json>, minimum: u64) -> Option<u64> {
    let number = value?.as_f64()?;
    (number.fract() == 0.0 && number >= minimum as f64).then(|| value.and_then(Json::as_u64).unwrap_or(u64::MAX))
}

/// Parse `{ key, generation }`.
pub fn parse_address(value: &Json) -> ChordResult<ServiceInstanceAddress> {
    let address = record(value, "service instance address")?;
    assert_keys(address, &["key", "generation"], &[], "service instance address")?;
    let key = address.get("key").and_then(id);
    let generation = integer(address.get("generation"), 1);
    match (key, generation) {
        (Some(key), Some(generation)) => Ok(ServiceInstanceAddress { key: key.to_string(), generation }),
        _ => Err(ChordError::type_error("Invalid service instance address")),
    }
}

fn optional_address(map: &JsonMap) -> ChordResult<Option<ServiceInstanceAddress>> {
    map.get("instance").map(parse_address).transpose()
}

/// Parse and validate a service call envelope.
pub fn parse_service_call(value: &Json) -> ChordResult<ServiceCall> {
    let call = record(value, "service call")?;
    assert_keys(call, &["serviceId", "member", "args"], &["instance"], "service call")?;
    let service_id = call.get("serviceId").and_then(id);
    let member = call.get("member").and_then(id);
    let args = call.get("args").and_then(Json::as_array);
    let (Some(service_id), Some(member), Some(args)) = (service_id, member, args) else {
        return Err(ChordError::type_error("Invalid service call"));
    };
    Ok(ServiceCall {
        service_id: service_id.to_string(),
        instance: optional_address(call)?,
        member: member.to_string(),
        args: args.clone(),
    })
}

/// Parse a catalogue: unique non-empty service IDs with valid modes.
pub fn parse_service_catalogue(value: &Json) -> ChordResult<Vec<ServiceCatalogueEntry>> {
    let items = value.as_array().ok_or_else(|| ChordError::type_error("Invalid service catalogue"))?;
    let mut entries: Vec<ServiceCatalogueEntry> = Vec::with_capacity(items.len());
    for candidate in items.iter() {
        let entry = record(candidate, "service catalogue entry")?;
        assert_keys(entry, &["serviceId", "mode"], &[], "service catalogue entry")?;
        let service_id = entry.get("serviceId").and_then(id);
        let mode = entry.get("mode").and_then(Json::as_str).and_then(ServiceMode::parse);
        match (service_id, mode) {
            (Some(service_id), Some(mode)) if !entries.iter().any(|existing| existing.service_id == service_id) => {
                entries.push(ServiceCatalogueEntry { service_id: service_id.to_string(), mode });
            }
            _ => return Err(ChordError::type_error("Invalid service catalogue")),
        }
    }
    Ok(entries)
}

fn parse_instance<O: OpForm>(value: &Json) -> ChordResult<ServiceInstanceSnapshotOf<O>> {
    let instance = record(value, "service instance snapshot")?;
    assert_keys(instance, &["members"], &["instance"], "service instance snapshot")?;
    let address = optional_address(instance)?;
    let members = instance
        .get("members")
        .and_then(Json::as_array)
        .ok_or_else(|| ChordError::type_error("Invalid service instance snapshot"))?;
    let mut parsed = Vec::with_capacity(members.len());
    for candidate in members.iter() {
        let member = record(candidate, "service member snapshot")?;
        match member.get("kind").and_then(Json::as_str) {
            Some("method") => {
                assert_keys(member, &["name", "kind"], &[], "service method snapshot")?;
                let name = member.get("name").and_then(id);
                let name = name.ok_or_else(|| ChordError::type_error("Invalid service method snapshot"))?;
                parsed.push(ServiceMemberSnapshotOf::Method { name: name.to_string() });
            }
            Some("state") => {
                assert_keys(member, &["name", "kind", "sequence", "ops"], &[], "service state snapshot")?;
                let name = member.get("name").and_then(id);
                let sequence = integer(member.get("sequence"), 0);
                let ops = member.get("ops").and_then(Json::as_array);
                let (Some(name), Some(sequence), Some(ops)) = (name, sequence, ops) else {
                    return Err(ChordError::type_error("Invalid service state snapshot"));
                };
                let ops = ops.iter().map(O::parse).collect::<ChordResult<Vec<_>>>()?;
                parsed.push(ServiceMemberSnapshotOf::State { name: name.to_string(), sequence, ops });
            }
            _ => return Err(ChordError::type_error("Invalid service member snapshot")),
        }
    }
    Ok(ServiceInstanceSnapshotOf { instance: address, members: parsed })
}

fn parse_snapshot<O: OpForm>(value: &Json) -> ChordResult<ServiceSubscriptionSnapshotOf<O>> {
    let snapshot = record(value, "service subscription snapshot")?;
    assert_keys(snapshot, &["serviceId", "mode", "instances"], &[], "service subscription snapshot")?;
    let service_id = snapshot.get("serviceId").and_then(id);
    let mode = snapshot.get("mode").and_then(Json::as_str).and_then(ServiceMode::parse);
    let instances = snapshot.get("instances").and_then(Json::as_array);
    let (Some(service_id), Some(mode), Some(instances)) = (service_id, mode, instances) else {
        return Err(ChordError::type_error("Invalid service subscription snapshot"));
    };
    let instances = instances.iter().map(parse_instance).collect::<ChordResult<Vec<_>>>()?;
    Ok(ServiceSubscriptionSnapshotOf { service_id: service_id.to_string(), mode, instances })
}

fn parse_update<O: OpForm>(value: &Json) -> ChordResult<ServiceProviderUpdateOf<O>> {
    let update = record(value, "service provider update")?;
    match update.get("type").and_then(Json::as_str) {
        Some("state") => {
            assert_keys(update, &["type", "member", "sequence", "ops"], &["instance"], "state update")?;
            let member = update.get("member").and_then(id);
            let sequence = integer(update.get("sequence"), 1);
            let ops = update.get("ops").and_then(Json::as_array);
            let (Some(member), Some(sequence), Some(ops)) = (member, sequence, ops) else {
                return Err(ChordError::type_error("Invalid service state update"));
            };
            let instance = optional_address(update)?;
            let ops = ops.iter().map(O::parse).collect::<ChordResult<Vec<_>>>()?;
            Ok(ServiceProviderUpdateOf::State { instance, member: member.to_string(), sequence, ops })
        }
        Some("reset") => {
            assert_keys(update, &["type", "snapshot"], &[], "reset update")?;
            let snapshot = parse_snapshot::<O>(update.get("snapshot").unwrap_or(&Json::Null))?;
            for instance in &snapshot.instances {
                for member in &instance.members {
                    if let ServiceMemberSnapshotOf::State { ops, .. } = member
                        && (ops.len() != 1 || !ops[0].is_replace())
                    {
                        return Err(ChordError::type_error("Service reset must contain full root replacements"));
                    }
                }
            }
            Ok(ServiceProviderUpdateOf::Reset { snapshot })
        }
        Some("unavailable") => {
            assert_keys(update, &["type"], &[], "unavailable update")?;
            Ok(ServiceProviderUpdateOf::Unavailable)
        }
        Some("replaced") => {
            assert_keys(update, &["type", "snapshot"], &[], "replacement update")?;
            Ok(ServiceProviderUpdateOf::Replaced {
                snapshot: parse_instance(update.get("snapshot").unwrap_or(&Json::Null))?,
            })
        }
        Some("spawned") => {
            assert_keys(update, &["type", "instance"], &[], "spawn update")?;
            Ok(ServiceProviderUpdateOf::Spawned {
                instance: parse_instance(update.get("instance").unwrap_or(&Json::Null))?,
            })
        }
        Some("closed") => {
            assert_keys(update, &["type", "instance"], &[], "close update")?;
            Ok(ServiceProviderUpdateOf::Closed {
                instance: parse_address(update.get("instance").unwrap_or(&Json::Null))?,
            })
        }
        _ => Err(ChordError::type_error("Invalid service provider update")),
    }
}

/// Parse a decoded subscription snapshot.
pub fn parse_service_subscription_snapshot(value: &Json) -> ChordResult<ServiceSubscriptionSnapshot> {
    parse_snapshot::<Op>(value)
}

/// Parse an encoded (wire) subscription snapshot.
pub fn parse_wire_service_subscription_snapshot(value: &Json) -> ChordResult<WireServiceSubscriptionSnapshot> {
    parse_snapshot::<WireOp>(value)
}

/// Parse a decoded provider update.
pub fn parse_service_provider_update(value: &Json) -> ChordResult<ServiceProviderUpdate> {
    parse_update::<Op>(value)
}

/// Parse an encoded (wire) provider update.
pub fn parse_wire_service_provider_update(value: &Json) -> ChordResult<WireServiceProviderUpdate> {
    parse_update::<WireOp>(value)
}
