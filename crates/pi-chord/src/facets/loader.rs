//! Facet loaders: produce a facet set plus the cleanup for whatever loaded it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::Facet;
use super::kernel::aggregate;
use super::lifecycle::single_or_aggregate;
use crate::error::{ChordError, ChordResult};
use crate::sync::{BoxFuture, lock};

type DisposeFn = Box<dyn FnOnce() -> BoxFuture<ChordResult<()>> + Send>;

/// Facets from one load, with the cleanup for their code and resources.
pub struct LoadedFacets {
    pub facets: Vec<Arc<dyn Facet>>,
    dispose: Mutex<Option<DisposeFn>>,
}

impl LoadedFacets {
    pub fn new<F, Fut>(facets: Vec<Arc<dyn Facet>>, dispose: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ChordResult<()>> + Send + 'static,
    {
        Self { facets, dispose: Mutex::new(Some(Box::new(move || Box::pin(dispose())))) }
    }

    /// Facets with nothing to clean up.
    pub fn fixed(facets: Vec<Arc<dyn Facet>>) -> Self {
        Self { facets, dispose: Mutex::new(None) }
    }

    /// Run the cleanup once; later calls do nothing.
    pub async fn dispose(&self) -> ChordResult<()> {
        let dispose = lock(&self.dispose).take();
        match dispose {
            Some(dispose) => dispose().await,
            None => Ok(()),
        }
    }
}

#[async_trait]
pub trait FacetLoader: Send + Sync {
    async fn load(&self) -> ChordResult<LoadedFacets>;
}

struct StaticFacetLoader(Vec<Arc<dyn Facet>>);

#[async_trait]
impl FacetLoader for StaticFacetLoader {
    async fn load(&self) -> ChordResult<LoadedFacets> {
        Ok(LoadedFacets::fixed(self.0.clone()))
    }
}

/// A loader that always returns the same facets.
pub fn create_static_facet_loader(facets: Vec<Arc<dyn Facet>>) -> Arc<dyn FacetLoader> {
    Arc::new(StaticFacetLoader(facets))
}

struct CombinedFacetLoader(Vec<Arc<dyn FacetLoader>>);

async fn dispose_loaded(loaded: &[Arc<LoadedFacets>]) -> Vec<ChordError> {
    let results = futures::future::join_all(loaded.iter().map(|entry| entry.dispose())).await;
    results.into_iter().filter_map(Result::err).collect()
}

#[async_trait]
impl FacetLoader for CombinedFacetLoader {
    async fn load(&self) -> ChordResult<LoadedFacets> {
        let mut loaded: Vec<Arc<LoadedFacets>> = Vec::new();
        for loader in &self.0 {
            match loader.load().await {
                Ok(entry) => loaded.push(Arc::new(entry)),
                Err(error) => {
                    loaded.reverse();
                    let cleanup = dispose_loaded(&loaded).await;
                    return Err(aggregate(error, cleanup, "Facet loading and cleanup failed"));
                }
            }
        }
        let facets = loaded.iter().flat_map(|entry| entry.facets.clone()).collect();
        Ok(LoadedFacets::new(facets, move || async move {
            let reversed: Vec<_> = loaded.into_iter().rev().collect();
            single_or_aggregate(dispose_loaded(&reversed).await, "Failed to dispose loaded facets")
        }))
    }
}

/// Load each loader in order and concatenate their facets; cleanup runs in reverse.
pub fn combine_facet_loaders(loaders: Vec<Arc<dyn FacetLoader>>) -> Arc<dyn FacetLoader> {
    Arc::new(CombinedFacetLoader(loaders))
}
