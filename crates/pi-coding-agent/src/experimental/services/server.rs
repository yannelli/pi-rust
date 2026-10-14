//! Server-scoped services for each presentation connection (upstream `services/server.ts`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::services::{
    RemoteServiceEndpoint, RemoteServiceProvider, ServiceCall, ServiceDefinition, ServiceImpl, ServiceUpdatePublisher,
    create_remote_service_endpoint,
};
use pi_chord::state::MutableReplicatedState;
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Context, Json};
use pi_server::{RoutedServerPresentation, RoutedServerServiceAttachment, RoutedServerServiceHost};

use super::codec::encode;
use super::plugins::{PRESENTATION_PLUGINS, PrepareSessionRequest, PresentationPlugins, presentation_plugins_impl};
use super::sessions::{
    SESSION_DIRECTORY, SESSION_MANAGEMENT, SessionCreateOptions, SessionDirectoryState, SessionManagement,
    SessionSummary, session_management_impl,
};
use crate::experimental::process::lock;

/// The plugin selection prepared for one Session.
pub struct PreparedSessionPlugins {
    pub package_paths: Vec<String>,
    pub presentation_plugins: Json,
}

/// What the server services delegate to the server runtime.
#[async_trait]
pub trait ServerServicesBackend: Send + Sync {
    async fn list(&self, context: &Context) -> ChordResult<Vec<SessionSummary>>;
    async fn create(&self, options: SessionCreateOptions, context: &Context) -> ChordResult<SessionSummary>;
    async fn remove(&self, session_id: &str, context: &Context) -> ChordResult<()>;
    async fn prepare_session_plugins(
        &self,
        session_id: &str,
        package_paths: Option<Vec<String>>,
        context: &Context,
    ) -> ChordResult<PreparedSessionPlugins>;
    async fn reload_presentation_plugins(&self, package_paths: Vec<String>, context: &Context) -> ChordResult<Json>;
}

struct Shared {
    backend: Arc<dyn ServerServicesBackend>,
    directory: MutableReplicatedState,
    revision: AtomicU64,
    mutations: tokio::sync::Mutex<()>,
    attachments: Mutex<Vec<Arc<ProviderAttachment>>>,
}

impl Shared {
    async fn refresh_now(&self, context: &Context) -> ChordResult<()> {
        let sessions = self.backend.list(context).await?;
        let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let value = encode(&SessionDirectoryState { revision, sessions })?;
        self.directory.change(context, move |draft| {
            *draft = value;
            Ok(())
        })
    }
}

/// The server service host plus refresh and disposal.
pub struct ExperimentalServerServices {
    shared: Arc<Shared>,
}

impl ExperimentalServerServices {
    pub fn host(&self) -> Arc<dyn RoutedServerServiceHost> {
        Arc::new(Host(self.shared.clone()))
    }

    pub async fn refresh(&self, context: &Context) -> ChordResult<()> {
        let _serialized = self.shared.mutations.lock().await;
        self.shared.refresh_now(context).await
    }

    pub async fn dispose(&self) -> ChordResult<()> {
        let attachments: Vec<Arc<ProviderAttachment>> = std::mem::take(&mut *lock(&self.shared.attachments));
        let mut errors = Vec::new();
        for attachment in attachments {
            if let Err(error) = attachment.release_now() {
                errors.push(error);
            }
        }
        drop(self.shared.mutations.lock().await);
        ChordError::collect(errors, "Failed to release server service attachments")
    }
}

pub async fn create_experimental_server_services(
    backend: Arc<dyn ServerServicesBackend>,
) -> ChordResult<ExperimentalServerServices> {
    let sessions = backend.list(&BACKGROUND_CONTEXT).await?;
    let directory = MutableReplicatedState::new(encode(&SessionDirectoryState { revision: 1, sessions })?);
    let shared = Arc::new(Shared {
        backend,
        directory,
        revision: AtomicU64::new(1),
        mutations: tokio::sync::Mutex::new(()),
        attachments: Mutex::default(),
    });
    Ok(ExperimentalServerServices { shared })
}

struct Host(Arc<Shared>);

#[async_trait]
impl RoutedServerServiceHost for Host {
    async fn attach_client(
        &self,
        presentation: RoutedServerPresentation,
        _context: &Context,
    ) -> ChordResult<Arc<dyn RoutedServerServiceAttachment>> {
        let definitions =
            [&*SESSION_DIRECTORY, &*SESSION_MANAGEMENT, &*PRESENTATION_PLUGINS].map(ServiceDefinition::singleton);
        let provider = RemoteServiceProvider::new(&definitions)?;
        let prepared = Arc::new(Mutex::new(None));
        provider.provide(&SESSION_DIRECTORY, ServiceImpl::new().state("state", &self.0.directory))?;
        let plugins = Arc::new(Plugins { shared: self.0.clone(), prepared: prepared.clone() });
        provider.provide(&PRESENTATION_PLUGINS, presentation_plugins_impl(plugins))?;
        let management = Arc::new(Management { shared: self.0.clone(), presentation, prepared });
        provider.provide(&SESSION_MANAGEMENT, session_management_impl(management))?;
        let endpoint = create_remote_service_endpoint(&provider);
        let attachment = Arc::new(ProviderAttachment {
            shared: Arc::downgrade(&self.0),
            provider,
            endpoint,
            released: AtomicBool::new(false),
        });
        lock(&self.0.attachments).push(attachment.clone());
        Ok(attachment)
    }
}

struct Plugins {
    shared: Arc<Shared>,
    prepared: Arc<Mutex<Option<Vec<String>>>>,
}

#[async_trait]
impl PresentationPlugins for Plugins {
    async fn prepare_session(&self, request: PrepareSessionRequest, context: &Context) -> ChordResult<Json> {
        let _serialized = self.shared.mutations.lock().await;
        let backend = &self.shared.backend;
        let selected = backend.prepare_session_plugins(&request.session_id, request.package_paths, context).await?;
        *lock(&self.prepared) = Some(selected.package_paths);
        Ok(selected.presentation_plugins)
    }

    async fn reload(&self, context: &Context) -> ChordResult<Json> {
        let _serialized = self.shared.mutations.lock().await;
        let prepared = lock(&self.prepared).clone();
        let Some(package_paths) = prepared else {
            return Err(ChordError::error("No Session plugin selection is prepared"));
        };
        self.shared.backend.reload_presentation_plugins(package_paths, context).await
    }
}

struct Management {
    shared: Arc<Shared>,
    presentation: RoutedServerPresentation,
    prepared: Arc<Mutex<Option<Vec<String>>>>,
}

#[async_trait]
impl SessionManagement for Management {
    async fn create(&self, options: SessionCreateOptions, context: &Context) -> ChordResult<SessionSummary> {
        let _serialized = self.shared.mutations.lock().await;
        let created = self.shared.backend.create(options, context).await?;
        self.shared.refresh_now(context).await?;
        Ok(created)
    }

    async fn remove(&self, session_id: String, context: &Context) -> ChordResult<()> {
        let _serialized = self.shared.mutations.lock().await;
        self.presentation.prepare_session_removal(&session_id, context).await?;
        self.shared.backend.remove(&session_id, context).await?;
        self.shared.refresh_now(context).await
    }

    async fn attach(&self, session_id: String, context: &Context) -> ChordResult<()> {
        let _serialized = self.shared.mutations.lock().await;
        self.presentation.attach_session(&session_id, context).await
    }

    async fn detach(&self, context: &Context) -> ChordResult<()> {
        let _serialized = self.shared.mutations.lock().await;
        self.presentation.detach_session(context).await?;
        *lock(&self.prepared) = None;
        Ok(())
    }
}

struct ProviderAttachment {
    shared: std::sync::Weak<Shared>,
    provider: RemoteServiceProvider,
    endpoint: RemoteServiceEndpoint,
    released: AtomicBool,
}

impl ProviderAttachment {
    fn release_now(&self) -> ChordResult<()> {
        if self.released.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.endpoint.dispose();
        let disposed = self.provider.dispose();
        if let Some(shared) = self.shared.upgrade() {
            lock(&shared.attachments).retain(|attachment| !std::ptr::eq(Arc::as_ptr(attachment), self));
        }
        disposed
    }
}

#[async_trait]
impl RoutedServerServiceAttachment for ProviderAttachment {
    async fn invoke_service(
        &self,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        if self.released.load(Ordering::SeqCst) {
            return Err(ChordError::error("Server service attachment is released"));
        }
        self.endpoint.invoke(call, publish, context).await
    }

    async fn release(&self, _context: &Context) -> ChordResult<()> {
        self.release_now()
    }
}
