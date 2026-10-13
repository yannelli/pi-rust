//! Replacing facets in place without disconnecting consumer handles.

use std::collections::HashMap;
use std::sync::Arc;

use super::Facet;
use super::environment::FacetRuntime;
use super::graph::same_facet_shape;
use super::kernel::{FacetKernel, Phase, aggregate, check_ids, dispose_records};
use super::lifecycle::single_or_aggregate;
use super::spawner::{FacetProvision, SingletonValue};
use crate::error::{ChordError, ChordResult};
use crate::services::handle::SlotTarget;
use crate::sync::lock;

impl FacetKernel {
    fn validate_replacements(&self, record: &FacetRuntime) -> ChordResult<()> {
        for provision in record.provisions() {
            if let FacetProvision::Singleton { service, value: SingletonValue::Remote(implementation) } = provision {
                self.provider()?.validate_replacement(&service, &implementation)?;
            }
        }
        Ok(())
    }

    fn stage(&self, facets: &[Arc<dyn Facet>], staged: &mut Vec<Arc<FacetRuntime>>) -> ChordResult<()> {
        for facet in facets {
            let record = FacetRuntime::new(facet.id());
            staged.push(record.clone());
            self.setup_facet(facet, &record)?;
            let previous = lock(&self.state).facets.get(facet.id()).cloned();
            if !previous.is_some_and(|previous| same_facet_shape(&previous, &record)) {
                return Err(ChordError::error(format!(
                    "Reloaded facet {} must preserve its service requirements and provisions",
                    facet.id()
                )));
            }
            self.validate_replacements(&record)?;
        }
        Ok(())
    }

    async fn activate_candidates(&self, order: &[Arc<FacetRuntime>]) -> ChordResult<()> {
        for candidate in order {
            candidate.lifecycle.activate().await?;
        }
        for candidate in order {
            self.validate_replacements(candidate)?;
        }
        Ok(())
    }

    async fn cut_over(&self, order: &[Arc<FacetRuntime>], previous: &[Arc<FacetRuntime>]) -> ChordResult<()> {
        let provider = self.provider()?;
        for candidate in order {
            for provision in candidate.provisions() {
                match provision {
                    FacetProvision::Singleton { service, value: SingletonValue::Local(implementation) } => {
                        self.slots.bind_singleton(service.id(), SlotTarget::Local(implementation));
                    }
                    FacetProvision::Singleton { service, value: SingletonValue::Remote(implementation) } => {
                        provider.replace(&service, implementation)?;
                    }
                    FacetProvision::Keyed { .. } => {}
                }
            }
        }
        let retirement = dispose_records(previous.iter().rev().cloned()).await;
        single_or_aggregate(retirement, "Failed to retire replaced facets")?;
        for candidate in order {
            for provision in candidate.provisions() {
                match &provision {
                    FacetProvision::Keyed { service, .. } if service.is_local() => {
                        provision.connect_local(&self.local_keyed()?)?;
                    }
                    FacetProvision::Keyed { .. } => provision.connect_remote(&provider)?,
                    FacetProvision::Singleton { .. } => {}
                }
            }
        }
        Ok(())
    }

    pub async fn reload(&self, facets: Vec<Arc<dyn Facet>>) -> ChordResult<()> {
        let phase = self.phase();
        if phase != Phase::Active {
            return Err(ChordError::error(format!("Facet host cannot reload while {phase}")));
        }
        check_ids(facets.iter().map(|facet| facet.id()), "Reloaded facet IDs must be unique")?;
        for facet in &facets {
            if !lock(&self.state).facets.contains_key(facet.id()) {
                return Err(ChordError::error(format!("Facet {} is not active", facet.id())));
            }
        }
        self.set_phase(Phase::Reloading);

        let mut staged = Vec::new();
        if let Err(error) = self.stage(&facets, &mut staged) {
            let cleanup = dispose_records(staged.into_iter().rev()).await;
            if !cleanup.is_empty() {
                let mut cleanup = cleanup;
                cleanup.extend(self.abort(Vec::new()).await);
                return Err(aggregate(error, cleanup, "Facet reload setup and cleanup failed"));
            }
            self.set_phase(Phase::Active);
            return Err(error);
        }

        let candidates: HashMap<String, Arc<FacetRuntime>> =
            staged.iter().map(|record| (record.facet_id.clone(), record.clone())).collect();
        let order: Vec<Arc<FacetRuntime>> = {
            let state = lock(&self.state);
            state.activation_order.iter().filter_map(|id| candidates.get(id).cloned()).collect()
        };
        if let Err(error) = self.activate_candidates(&order).await {
            let mut cleanup = dispose_records(order.iter().rev().cloned()).await;
            if !cleanup.is_empty() {
                cleanup.extend(self.abort(Vec::new()).await);
                return Err(aggregate(error, cleanup, "Facet reload activation and cleanup failed"));
            }
            self.set_phase(Phase::Active);
            return Err(error);
        }

        let previous: Vec<Arc<FacetRuntime>> = {
            let mut state = lock(&self.state);
            let previous = order.iter().filter_map(|record| state.facets.get(&record.facet_id).cloned()).collect();
            for candidate in &order {
                state.facets.insert(candidate.facet_id.clone(), candidate.clone());
            }
            previous
        };
        if let Err(error) = self.cut_over(&order, &previous).await {
            let cleanup = self.abort(previous).await;
            let mut errors = vec![error];
            errors.extend(cleanup);
            return Err(ChordError::Aggregate { message: "Facet reload failed after cutover".into(), errors });
        }
        self.set_phase(Phase::Active);
        Ok(())
    }
}
