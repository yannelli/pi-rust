//! Application capabilities the server routes to, and server options.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_chord::services::{ServiceCall, ServiceUpdatePublisher};
use pi_chord::{BoxFuture, ChordError, ChordResult, Context, Json};

use crate::listener::ServerListener;

/// Receives failures that have no other owner.
pub type ErrorReporter = Arc<dyn Fn(&ChordError) + Send + Sync>;

pub struct ServerOptions {
    pub listeners: Vec<Arc<dyn ServerListener>>,
    /// Stable logical server identity supplied by the installation or profile.
    pub server_id: String,
    pub max_frame_length: Option<u32>,
    pub handshake_timeout: Option<Duration>,
    pub on_connection_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    pub on_error: Option<ErrorReporter>,
}

impl ServerOptions {
    pub fn new(server_id: impl Into<String>, listeners: Vec<Arc<dyn ServerListener>>) -> Self {
        Self {
            listeners,
            server_id: server_id.into(),
            max_frame_length: None,
            handshake_timeout: None,
            on_connection_count_changed: None,
            on_error: None,
        }
    }
}

/// Minimal durable Session metadata the server needs for routing. Applications may extend it.
pub trait SessionMetadata: Send + Sync + 'static {
    fn id(&self) -> &str;
}

/// Metadata carrying only the Session ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasicSessionMetadata {
    pub id: String,
}

impl SessionMetadata for BasicSessionMetadata {
    fn id(&self) -> &str {
        &self.id
    }
}

/// One connection's capability for a server-wide or Session service endpoint.
#[async_trait]
pub trait RoutedServiceAttachment: Send + Sync {
    /// Route one contract-agnostic service operation to the endpoint.
    async fn invoke_service(
        &self,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: &Context,
    ) -> ChordResult<Option<Json>>;
    async fn release(&self, context: &Context) -> ChordResult<()>;
}

pub use RoutedServiceAttachment as RoutedSessionAttachment;
pub use RoutedServiceAttachment as RoutedServerServiceAttachment;

pub(crate) trait PresentationOps: Send + Sync {
    fn attach(&self, client: u64, session_id: String, context: Context) -> BoxFuture<ChordResult<()>>;
    fn detach(&self, client: u64, context: Context) -> BoxFuture<ChordResult<()>>;
    fn remove(&self, session_id: String, context: Context) -> BoxFuture<ChordResult<()>>;
}

/// Presentation-scoped routing capabilities available to server service implementations.
#[derive(Clone)]
pub struct RoutedServerPresentation {
    pub(crate) client: u64,
    pub(crate) ops: Arc<dyn PresentationOps>,
}

impl RoutedServerPresentation {
    pub async fn attach_session(&self, session_id: &str, context: &Context) -> ChordResult<()> {
        self.ops.attach(self.client, session_id.to_string(), context.clone()).await
    }

    pub async fn detach_session(&self, context: &Context) -> ChordResult<()> {
        self.ops.detach(self.client, context.clone()).await
    }

    /// Release routed attachments and handles before the application deletes durable metadata.
    pub async fn prepare_session_removal(&self, session_id: &str, context: &Context) -> ChordResult<()> {
        self.ops.remove(session_id.to_string(), context.clone()).await
    }
}

/// Creates one connection-scoped server service endpoint.
#[async_trait]
pub trait RoutedServerServiceHost: Send + Sync {
    async fn attach_client(
        &self,
        presentation: RoutedServerPresentation,
        context: &Context,
    ) -> ChordResult<Arc<dyn RoutedServerServiceAttachment>>;
}

/// A process-safe handle that acquires presentation-scoped Session capabilities.
#[async_trait]
pub trait RoutedSessionHandle: Send + Sync {
    async fn attach_client(&self, context: &Context) -> ChordResult<Arc<dyn RoutedSessionAttachment>>;
    /// Resolves with an error when the Session stops unexpectedly, or `None` after an expected close.
    fn terminated(&self) -> Option<BoxFuture<Option<ChordError>>> {
        None
    }
    async fn close(&self, context: &Context) -> ChordResult<()>;
}

/// Application capabilities used by server-wide management and Session routing.
#[async_trait]
pub trait ServerHost: Send + Sync + 'static {
    type Metadata: SessionMetadata;
    fn server_services(&self) -> Arc<dyn RoutedServerServiceHost>;
    /// Resolve one durable Session ID or fail with a bounded routing error.
    async fn resolve_session(&self, session_id: &str, context: &Context) -> ChordResult<Self::Metadata>;
    async fn open_session(
        &self,
        metadata: Self::Metadata,
        context: &Context,
    ) -> ChordResult<Arc<dyn RoutedSessionHandle>>;
}
