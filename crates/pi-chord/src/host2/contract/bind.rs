//! Whether a provider's contract covers a consumer's, and how a consumer token binds to a provider.

use std::collections::HashMap;

use super::{Contract, Description, MemberDescription, placement_text};
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::ChordErrorCode;
use crate::host2::schema::{canonical_json, fits};

/// Adapters by consumer contract version: each maps the provider's current implementation to the view a consumer
/// of that older version expects. The adapter type belongs to the object layer.
pub type Accepts<A> = HashMap<i64, A>;

/// Returns why `provider` does not cover `consumer`, or `None` when it does.
pub fn coverage(provider: &Description, consumer: &Description) -> Option<String> {
    if provider.name() != consumer.name() || provider.scope() != consumer.scope() {
        return Some("different type".into());
    }
    if !same_placement(provider, consumer) {
        return Some("host".into());
    }
    if !fits(consumer.input(), provider.input()) {
        return Some("input".into());
    }
    for (kind, consumer_members) in consumer.variants() {
        let Some(provider_members) = provider.variant(kind) else {
            return Some(format!("variant {kind} missing"));
        };
        for (name, wanted) in consumer_members {
            let offered = match provider_members.get(name) {
                Some(offered) if offered.kind() == wanted.kind() => offered,
                _ => return Some(format!("{kind}.{name} missing or of another kind")),
            };
            if let Some(reason) = member_coverage(offered, wanted) {
                return Some(format!("{kind}.{name} {reason}"));
            }
        }
    }
    None
}

fn member_coverage(offered: &MemberDescription, wanted: &MemberDescription) -> Option<String> {
    match (offered, wanted) {
        // Both take arguments or neither does; a consumer that expects no result accepts any.
        (
            MemberDescription::Method { args: offered_args, result: offered_result },
            MemberDescription::Method { args: wanted_args, result: wanted_result },
        ) => {
            let arguments = match (wanted_args, offered_args) {
                (None, None) => true,
                (Some(wanted), Some(offered)) => fits(wanted, offered),
                _ => false,
            };
            if !arguments {
                return Some("arguments".into());
            }
            match (wanted_result, offered_result) {
                (Some(wanted), Some(offered)) if !fits(offered, wanted) => Some("result".into()),
                (Some(_), None) => Some("result".into()),
                _ => None,
            }
        }
        (MemberDescription::State { schema: offered }, MemberDescription::State { schema: wanted }) => {
            (!fits(offered, wanted)).then(|| "state".into())
        }
        (MemberDescription::Opaque { id: offered }, MemberDescription::Opaque { id: wanted }) => {
            (offered != wanted).then(|| format!("opaque {offered} != {wanted}"))
        }
        _ => None,
    }
}

fn same_placement(a: &Description, b: &Description) -> bool {
    canonical_json(&a.host_json()) == canonical_json(&b.host_json())
}

fn placement_of(description: &Description) -> String {
    description.host().map_or_else(|| "(local)".into(), placement_text)
}

fn mismatch(message: String) -> ChordError {
    ChordError::code_only(ChordErrorCode::ContractMismatch, message)
}

/// How a consumer token binds to a provider: directly (`None`) or through an `accepts` adapter.
/// Fails with `contract_mismatch`.
pub fn bind<'a, A>(
    provider: &Contract,
    accepts: Option<&'a Accepts<A>>,
    consumer: &Contract,
) -> ChordResult<Option<&'a A>> {
    if provider.fingerprint() == consumer.fingerprint() {
        return Ok(None);
    }
    // A moved type is another type for its consumers: no adapter reaches across hosts.
    let (offered, wanted) = (provider.description(), consumer.description());
    if !same_placement(offered, wanted) {
        let (from, to) = (placement_of(offered), placement_of(wanted));
        return Err(mismatch(format!("{} lives on {from}, the consumer expects {to}", provider.name())));
    }
    if provider.version() == consumer.version() {
        let Some(reason) = coverage(offered, wanted) else { return Ok(None) };
        return Err(mismatch(format!(
            "{}@{} does not cover the consumer's description ({reason}); forgotten version bump?",
            provider.name(),
            provider.version()
        )));
    }
    match accepts.and_then(|accepts| accepts.get(&consumer.version())) {
        Some(adapter) => Ok(Some(adapter)),
        None => Err(mismatch(format!(
            "{}: provider version {}, consumer version {}, no accepts adapter",
            provider.name(),
            provider.version(),
            consumer.version()
        ))),
    }
}

/// Whether the contract's method `name` takes no arguments (it is called as `ref.name(ctx)`).
pub fn takes_no_args(contract: &Contract, name: &str) -> bool {
    for members in contract.description().variants().values() {
        if let Some(MemberDescription::Method { args, .. }) = members.get(name) {
            return args.is_none();
        }
    }
    false
}
