//! Facet graph checks: unique providers, satisfied service needs, and activation order.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use indexmap::IndexMap;

use super::environment::{FacetRuntime, FacetServiceReference};
use crate::error::{ChordError, ChordResult};
use crate::services::{ServiceMode, ServiceRef};
use crate::sync::lock;

/// A needed service that a remote service source serves.
#[derive(Clone)]
pub(crate) struct ExternalService {
    pub service: ServiceRef,
    pub mode: ServiceMode,
    pub source: usize,
}

struct ProviderEntry {
    facet_id: Option<String>,
    mode: ServiceMode,
}

/// Check providers and service needs, then return the dependency-first activation order.
pub(crate) fn validate_facets(
    records: &[Arc<FacetRuntime>],
    external: &IndexMap<String, ExternalService>,
) -> ChordResult<Vec<String>> {
    let mut providers: HashMap<String, ProviderEntry> = HashMap::new();
    for (id, service) in external {
        providers.insert(id.clone(), ProviderEntry { facet_id: None, mode: service.mode });
    }
    for record in records {
        for provision in lock(&record.provides).iter() {
            let id = provision.service_id();
            if let Some(existing) = providers.get(id) {
                if existing.mode != provision.mode {
                    return Err(ChordError::error(format!("Service {id} is provided as both singleton and keyed")));
                }
                let message = match &existing.facet_id {
                    None => format!("Service {id} is provided by both the host and {}", record.facet_id),
                    Some(other) => format!("Service {id} is provided by both {other} and {}", record.facet_id),
                };
                return Err(ChordError::error(message));
            }
            let entry = ProviderEntry { facet_id: Some(record.facet_id.clone()), mode: provision.mode };
            providers.insert(id.to_string(), entry);
        }
    }

    let mut dependencies: HashMap<String, HashSet<String>> = HashMap::new();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    for record in records {
        dependencies.insert(record.facet_id.clone(), HashSet::new());
        dependents.insert(record.facet_id.clone(), Vec::new());
    }
    for record in records {
        for needed in lock(&record.requires).iter() {
            let (id, mode) = (needed.service_id(), needed.mode);
            let Some(provider) = providers.get(id) else {
                let message = format!("Facet {} requires local/{id}/{mode}, but no facet provides it", record.facet_id);
                return Err(ChordError::error(message));
            };
            if provider.mode != mode {
                let by = provider.facet_id.as_deref().unwrap_or("the host");
                let message = format!(
                    "Facet {} requires {id} as {mode}, but {by} provides it as {}",
                    record.facet_id, provider.mode
                );
                return Err(ChordError::error(message));
            }
            let Some(provider_id) = provider.facet_id.clone() else { continue };
            if provider_id == record.facet_id {
                continue;
            }
            if dependencies.get_mut(&record.facet_id).is_some_and(|set| set.insert(provider_id.clone())) {
                dependents.entry(provider_id).or_default().push(record.facet_id.clone());
            }
        }
    }
    topological_order(records, &dependencies, &dependents)
}

fn topological_order(
    records: &[Arc<FacetRuntime>],
    dependencies: &HashMap<String, HashSet<String>>,
    dependents: &HashMap<String, Vec<String>>,
) -> ChordResult<Vec<String>> {
    let mut remaining: HashMap<&str, usize> =
        dependencies.iter().map(|(id, values)| (id.as_str(), values.len())).collect();
    let mut ready: VecDeque<String> = records
        .iter()
        .map(|record| record.facet_id.clone())
        .filter(|id| remaining.get(id.as_str()) == Some(&0))
        .collect();
    let mut order = Vec::new();
    while let Some(id) = ready.pop_front() {
        for dependent in dependents.get(&id).into_iter().flatten() {
            let count = remaining.get_mut(dependent.as_str()).map(|count| {
                *count -= 1;
                *count
            });
            if count == Some(0) {
                ready.push_back(dependent.clone());
            }
        }
        order.push(id);
    }
    if order.len() != records.len() {
        let cycle: Vec<&str> = records
            .iter()
            .map(|record| record.facet_id.as_str())
            .filter(|id| remaining.get(id).is_some_and(|count| *count > 0))
            .collect();
        return Err(ChordError::error(format!("Facet dependency cycle: {}", cycle.join(", "))));
    }
    Ok(order)
}

fn same_references(left: &[FacetServiceReference], right: &[FacetServiceReference]) -> bool {
    left.len() == right.len()
        && left.iter().all(|reference| {
            right.iter().any(|other| other.service_id() == reference.service_id() && other.mode == reference.mode)
        })
}

pub(crate) fn same_facet_shape(left: &FacetRuntime, right: &FacetRuntime) -> bool {
    same_references(&lock(&left.requires), &lock(&right.requires))
        && same_references(&lock(&left.provides), &lock(&right.provides))
}
