//! Consumer-side checks of full reset snapshots.

use std::collections::HashSet;

use super::wire::{ServiceMemberSnapshotOf, ServiceMode, ServiceSubscriptionSnapshot};
use crate::error::{ChordError, ChordResult};

pub(crate) fn validate_reset_snapshot(
    snapshot: &ServiceSubscriptionSnapshot,
    service_id: &str,
    mode: ServiceMode,
) -> ChordResult<()> {
    if snapshot.service_id != service_id
        || snapshot.mode != mode
        || (mode == ServiceMode::Singleton && snapshot.instances.len() > 1)
    {
        return Err(ChordError::error("Remote service reset has the wrong service or mode"));
    }
    let mut keys = HashSet::new();
    for instance in &snapshot.instances {
        if (mode == ServiceMode::Singleton) == instance.instance.is_some() {
            return Err(ChordError::error("Remote service reset has an invalid instance address"));
        }
        if let Some(address) = &instance.instance
            && !keys.insert(address.key.clone())
        {
            return Err(ChordError::error("Remote service reset repeats an instance key"));
        }
        for member in &instance.members {
            if let ServiceMemberSnapshotOf::State { ops, .. } = member
                && (ops.len() != 1 || !ops[0].is_replace())
            {
                return Err(ChordError::error("Remote service reset must contain full root replacements"));
            }
        }
    }
    Ok(())
}
