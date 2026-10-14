//! Port of upstream `test/experimental-service-binding.ts`: bind server or selected-Session services
//! without a facet host.

use std::sync::{Arc, Mutex};

use pi_chord::services::{
    RemoteServiceSource, RemoteServiceSourceOpenOptions, RemoteServices, ServiceHandle, ServiceRef,
};
use pi_chord::{ChordError, ChordResult, Context};
use pi_client::Client;
use pi_coding_agent::experimental::services::connection::{
    ServerServiceSource, SessionAttachmentState, SessionServiceSource,
};

pub type Errors = Arc<Mutex<Vec<String>>>;

fn open(source: &dyn RemoteServiceSource, services: &[&ServiceRef], errors: &Errors) -> Arc<dyn RemoteServices> {
    let errors = errors.clone();
    source
        .open(RemoteServiceSourceOpenOptions {
            services: services.iter().map(|service| (*service).clone()).collect(),
            assert_access: Arc::new(|| Ok(())),
            on_error: Arc::new(move |error| errors.lock().unwrap().push(error.to_string())),
        })
        .unwrap()
}

fn reporter(errors: &Errors) -> pi_chord::state::ErrorReporter {
    let errors = errors.clone();
    Arc::new(move |error: ChordError| errors.lock().unwrap().push(error.to_string()))
}

fn collect(results: [ChordResult<()>; 2]) -> ChordResult<()> {
    ChordError::collect(results.into_iter().filter_map(Result::err).collect(), "Failed to dispose service binding")
}

pub struct ServerServiceBinding {
    pub source: Arc<ServerServiceSource>,
    pub services: Arc<dyn RemoteServices>,
}

pub fn create_server_service_binding(
    client: &Client,
    services: &[&ServiceRef],
    errors: &Errors,
) -> ServerServiceBinding {
    let source = ServerServiceSource::new(client, Some(reporter(errors))).unwrap();
    let services = open(source.as_ref(), services, errors);
    ServerServiceBinding { source, services }
}

impl ServerServiceBinding {
    pub fn use_service(&self, service: &ServiceRef) -> ServiceHandle {
        self.services.use_service(service).unwrap()
    }

    pub async fn ready(&self, context: &Context) -> ChordResult<()> {
        self.services.ready(context).await
    }

    pub async fn dispose(&self, context: &Context) -> ChordResult<()> {
        collect([self.services.dispose(context).await, self.source.dispose(context).await])
    }
}

pub struct SessionServiceBinding {
    pub source: Arc<SessionServiceSource>,
    pub services: Arc<dyn RemoteServices>,
}

pub fn create_session_service_binding(
    client: &Client,
    services: &[&ServiceRef],
    errors: &Errors,
) -> SessionServiceBinding {
    let source = SessionServiceSource::new(client, Some(reporter(errors))).unwrap();
    let services = open(source.as_ref(), services, errors);
    SessionServiceBinding { source, services }
}

impl SessionServiceBinding {
    pub fn use_service(&self, service: &ServiceRef) -> ServiceHandle {
        self.services.use_service(service).unwrap()
    }

    pub async fn ready(&self, context: &Context) -> ChordResult<()> {
        self.services.ready(context).await?;
        match self.source.attachment_state() {
            Some(SessionAttachmentState::Detached) => self.source.when_detached(context).await,
            Some(SessionAttachmentState::Attaching { session_id })
            | Some(SessionAttachmentState::Attached { session_id })
            | Some(SessionAttachmentState::Degraded { session_id }) => {
                self.source.when_attached(&session_id, context).await
            }
            None => Ok(()),
        }
    }

    pub async fn dispose(&self, context: &Context) -> ChordResult<()> {
        collect([self.services.dispose(context).await, self.source.dispose(context).await])
    }
}
