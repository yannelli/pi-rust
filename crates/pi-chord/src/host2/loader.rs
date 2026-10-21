//! Facet loading (upstream `loader.ts`). Upstream imports fresh copies of facet modules through Node module hooks,
//! which Rust cannot do. [`FacetLoader`] is the seam instead: a host's `load` and `reload` ask it for fresh facets
//! by module name. [`InProcessLoader`] serves facets compiled into this process (tests, embedded plugins).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{ChordError, ChordResult};
use crate::sync::lock;

/// Loads facet modules: fresh copies on every call, one facet per module, in order, each named by its module.
/// `F` is the host's facet type.
#[async_trait]
pub trait FacetLoader<F>: Send + Sync {
    async fn load(&self, modules: &[String]) -> ChordResult<Vec<F>>;
}

/// Builds a fresh facet named by the module it gets.
pub type ModuleFactory<F> = Arc<dyn Fn(&str) -> ChordResult<F> + Send + Sync>;

/// A [`FacetLoader`] over modules defined in this process. Redefining a module is how its code changes: the next
/// load builds the new version, and facets built earlier are unaffected.
pub struct InProcessLoader<F> {
    modules: Mutex<HashMap<String, ModuleFactory<F>>>,
    loads: AtomicU64,
}

impl<F> Default for InProcessLoader<F> {
    fn default() -> Self {
        Self { modules: Mutex::default(), loads: AtomicU64::new(0) }
    }
}

impl<F> InProcessLoader<F> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Defines (or redefines) `module`.
    pub fn define(&self, module: impl Into<String>, factory: impl Fn(&str) -> ChordResult<F> + Send + Sync + 'static) {
        lock(&self.modules).insert(module.into(), Arc::new(factory));
    }

    /// Whether `module` was defined.
    pub fn remove(&self, module: &str) -> bool {
        lock(&self.modules).remove(module).is_some()
    }

    /// How many loads succeeded in finding their modules (upstream's import generation).
    pub fn generation(&self) -> u64 {
        self.loads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl<F: Send + 'static> FacetLoader<F> for InProcessLoader<F> {
    async fn load(&self, modules: &[String]) -> ChordResult<Vec<F>> {
        // Every module first: one load builds one version of each, even while another is being redefined.
        let factories = {
            let table = lock(&self.modules);
            let found = modules.iter().map(|module| match table.get(module) {
                Some(factory) => Ok((module.as_str(), factory.clone())),
                None => Err(ChordError::type_error(format!("{module}: no facet module of that name"))),
            });
            found.collect::<ChordResult<Vec<_>>>()?
        };
        self.loads.fetch_add(1, Ordering::SeqCst);
        factories.into_iter().map(|(module, factory)| factory(module)).collect()
    }
}
