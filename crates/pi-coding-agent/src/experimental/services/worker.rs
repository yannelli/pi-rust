//! The Session worker's facet host and per-attachment service endpoints (upstream `services/worker.ts`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::facets::{
    Facet, FacetHost, FacetLoader, FacetOptions, LoadedFacets, create_facet_host, create_static_facet_loader,
    define_facet,
};
use pi_chord::services::{RemoteServiceEndpoint, ServiceCall, ServiceProviderUpdate, create_remote_service_endpoint};
use pi_chord::{BACKGROUND_CONTEXT, BoxFuture, ChordError, ChordResult, Context, Json};
use pi_durable::harness::{Conversation, Harness};

use super::agent_controller::{AGENT_CONTROLLER, agent_controller_impl, create_agent_controller};
use super::models::{ModelCatalog, ModelDefaults, create_models_service_facet};
use super::plugins::{SESSION_PLUGINS, SessionPlugins, session_plugins_impl};
use super::transcript::create_transcript_service_facet;
use crate::experimental::process::lock;
use crate::experimental::session_worker::WorkerOperationScope;

pub type RuntimeCleanup = Box<dyn FnOnce(Context) -> BoxFuture<ChordResult<()>> + Send>;

/// What a worker opens for its Session.
pub struct SessionWorkerRuntime {
    pub harness: Harness,
    /// The root conversation the services expose.
    pub conversation: Conversation,
    pub model_catalog: Option<Arc<dyn ModelCatalog>>,
    pub model_defaults: Option<Arc<dyn ModelDefaults>>,
    pub facet_loader: Option<Arc<dyn FacetLoader>>,
    /// Release resources the Harness does not own, such as execution environments, after it closed.
    pub cleanup: Option<RuntimeCleanup>,
}

/// Forwards one subscription update for one attachment scope, in order.
pub type WorkerServicePublisher = Arc<dyn Fn(&WorkerOperationScope, &str, ServiceProviderUpdate) + Send + Sync>;

type Reload = Arc<dyn Fn() -> BoxFuture<ChordResult<()>> + Send + Sync>;

struct PluginReload(Mutex<Option<Reload>>);

#[async_trait]
impl SessionPlugins for PluginReload {
    async fn reload(&self, _context: &Context) -> ChordResult<()> {
        let reload = lock(&self.0).clone();
        match reload {
            Some(reload) => reload().await,
            None => Err(ChordError::error("Session plugins are not ready")),
        }
    }
}

pub struct SessionWorkerServices {
    host: Arc<FacetHost>,
    builtins: LoadedFacets,
    plugins: Arc<tokio::sync::Mutex<Arc<LoadedFacets>>>,
    endpoints: Mutex<HashMap<WorkerOperationScope, Arc<RemoteServiceEndpoint>>>,
    publish: WorkerServicePublisher,
}

pub struct SessionWorkerServicesOptions<'a> {
    pub harness: &'a Harness,
    pub conversation: &'a Conversation,
    pub model_catalog: Option<Arc<dyn ModelCatalog>>,
    pub model_defaults: Option<Arc<dyn ModelDefaults>>,
    pub facet_loader: Option<Arc<dyn FacetLoader>>,
    pub publish: WorkerServicePublisher,
}

pub async fn create_session_worker_services(
    options: SessionWorkerServicesOptions<'_>,
) -> ChordResult<SessionWorkerServices> {
    let controller = create_agent_controller(options.harness.clone(), options.conversation.clone());
    let controller_facet = define_facet("@pi/agent-controller-runtime", move |env| {
        env.provide(&AGENT_CONTROLLER, agent_controller_impl(controller.clone()))
    });
    let reload = Arc::new(PluginReload(Mutex::new(None)));
    let plugin_service = reload.clone();
    let plugin_facet = define_facet("@pi/session-plugins-runtime", move |env| {
        env.provide(&SESSION_PLUGINS, session_plugins_impl(plugin_service.clone()))
    });
    let (harness, conversation, context) = (options.harness, options.conversation, &BACKGROUND_CONTEXT);
    let models =
        create_models_service_facet(harness, conversation, options.model_catalog, options.model_defaults, context);
    let builtin_facets: Vec<Arc<dyn Facet>> = vec![
        controller_facet,
        plugin_facet,
        models.await?,
        create_transcript_service_facet(conversation, context).await?,
    ];
    let builtins = create_static_facet_loader(builtin_facets).load().await?;
    let plugin_loader = options.facet_loader.unwrap_or_else(|| create_static_facet_loader(Vec::new()));
    let loaded = plugin_loader.load().await?;
    let facets = builtins.facets.iter().chain(&loaded.facets).cloned().collect();
    let host = match create_facet_host(FacetOptions::new(facets)).await {
        Ok(host) => Arc::new(host),
        Err(error) => {
            let cleanup: Vec<ChordError> =
                [loaded.dispose().await, builtins.dispose().await].into_iter().filter_map(Result::err).collect();
            if cleanup.is_empty() {
                return Err(error);
            }
            return Err(ChordError::with_cleanup(error, cleanup, "Session facets failed to start and clean up"));
        }
    };
    let plugins = Arc::new(tokio::sync::Mutex::new(Arc::new(loaded)));
    let (reload_host, reload_plugins) = (host.clone(), plugins.clone());
    *lock(&reload.0) = Some(Arc::new(move || {
        let (host, plugins, loader) = (reload_host.clone(), reload_plugins.clone(), plugin_loader.clone());
        Box::pin(async move {
            let mut current = plugins.lock().await;
            let candidate = loader.load().await?;
            if let Err(error) = host.reload(candidate.facets.clone()).await {
                return match candidate.dispose().await {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(ChordError::with_cleanup(error, vec![cleanup], "Session plugin reload and cleanup failed"))
                    }
                };
            }
            let retired = std::mem::replace(&mut *current, Arc::new(candidate));
            retired.dispose().await
        })
    }));
    Ok(SessionWorkerServices { host, builtins, plugins, endpoints: Mutex::default(), publish: options.publish })
}

impl SessionWorkerServices {
    pub async fn invoke(
        &self,
        call: ServiceCall,
        scope: &WorkerOperationScope,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        let endpoint = lock(&self.endpoints)
            .entry(scope.clone())
            .or_insert_with(|| Arc::new(create_remote_service_endpoint(self.host.services())))
            .clone();
        let (publish, scope) = (self.publish.clone(), scope.clone());
        let publisher = Arc::new(move |subscription_id: &str, update, _context: &Context| {
            publish(&scope, subscription_id, update);
        });
        endpoint.invoke(call, publisher, context).await
    }

    pub fn remove_subscriptions(&self, matches: impl Fn(&WorkerOperationScope) -> bool) {
        lock(&self.endpoints).retain(|scope, endpoint| {
            let remove = matches(scope);
            if remove {
                endpoint.dispose();
            }
            !remove
        });
    }

    pub async fn dispose(&self) -> ChordResult<()> {
        self.remove_subscriptions(|_| true);
        let plugins = self.plugins.lock().await.clone();
        let mut errors = Vec::new();
        for result in [self.host.dispose().await, plugins.dispose().await, self.builtins.dispose().await] {
            if let Err(error) = result {
                errors.push(error);
            }
        }
        ChordError::collect(errors, "Failed to dispose Session facets")
    }
}
