//! Port of upstream `experimental/client-runtime.ts`: open the server and Session service sources of every
//! selected server for one presentation.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::future::join_all;
use pi_chord::services::{
    RemoteServiceSource, RemoteServiceSourceOpenOptions, RemoteServices, ServiceHandle, ServiceRef,
};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Context};
use pi_client::unix::{DiscoverUnixServersOptions, UnixServerRoute, UnixTransportOptions, discover_unix_servers};
use pi_client::{Client, ClientOptions};
use pi_protocol::is_server_id;

use super::cli::{ClientCommand, TransportAddress};
use super::radius_auth::RadiusRelayAuthResolver;
use super::radius_client::{RadiusClientReconnect, RadiusReconnectClient, create_radius_client_transport_factory};
use super::server::{ENV_SERVER_ID, resolve_server_directory, resolve_session_directory};
use super::server_activation::{ActivateServerOptions, activate_server};
use super::services::agent_controller::{AGENT_CONTROLLER, AgentControllerClient};
use super::services::connection::{ServerServiceSource, SessionServiceSource};
use super::services::models::{MODELS, ModelsClient};
use super::services::plugins::{PRESENTATION_PLUGINS, PresentationPluginsClient};
use super::services::sessions::{
    SESSION_DIRECTORY, SESSION_MANAGEMENT, SessionCreateOptions, SessionManagement, SessionManagementClient,
    SessionSummary,
};
use super::services::transcript::TRANSCRIPT;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientRuntimeRoute {
    Unix(UnixServerRoute),
    Radius { server_id: String },
}

impl ClientRuntimeRoute {
    pub fn server_id(&self) -> &str {
        match self {
            Self::Unix(route) => &route.server_id,
            Self::Radius { server_id } => server_id,
        }
    }

    pub fn is_radius(&self) -> bool {
        matches!(self, Self::Radius { .. })
    }
}

#[derive(Clone)]
pub struct ClientRuntimeServer {
    pub route: ClientRuntimeRoute,
    pub client: Client,
    pub server: Arc<ServerServiceSource>,
    pub session: Arc<SessionServiceSource>,
}

#[derive(Default)]
pub struct ClientRuntime {
    pub servers: Vec<ClientRuntimeServer>,
    clients: Vec<Client>,
    server_sources: Vec<Arc<ServerServiceSource>>,
    session_sources: Vec<Arc<SessionServiceSource>>,
    reconnectors: Vec<RadiusClientReconnect>,
    disposed: AtomicBool,
}

/// Upstream `OpenClientRuntimeOptions`. The server ID and Session directory stand in for `PI_SERVER_ID` and
/// the agent directory, which Rust tests cannot stub.
#[derive(Clone, Debug, Default)]
pub struct OpenClientRuntimeOptions {
    /// Directory searched when `--connect` is omitted. Defaults to `PI_SERVER_DIR` or `~/.pi/server`.
    pub directory: Option<String>,
    pub requested_server_id: Option<String>,
    pub session_dir: Option<String>,
}

fn route_from_explicit_path(path: &str) -> ChordResult<UnixServerRoute> {
    let name = Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let server_id = name.strip_suffix(".sock").unwrap_or_default();
    if !is_server_id(server_id) {
        return Err(ChordError::error("--connect path must end with <uuidv4-server-id>.sock"));
    }
    Ok(UnixServerRoute { server_id: server_id.to_string(), path: path.into() })
}

fn validate(command: &ClientCommand) -> ChordResult<()> {
    let radius = matches!(command.connect, Some(TransportAddress::Radius { .. }));
    let error = if command.auth.is_some() && !radius {
        "Authentication is only supported for experimental Radius connections"
    } else if command.provider.is_some() && command.model.is_none() {
        "Server model provider requires a model"
    } else if command.connect.is_some() && command.model.is_some() {
        "Model selection is only valid when automatically activating a new server"
    } else if radius && command.plugin_packages.is_some() {
        "Plugin package paths can only be configured on a local Unix server"
    } else {
        return Ok(());
    };
    Err(ChordError::error(error))
}

/// Open live server and Session service sources for one experimental presentation.
pub async fn open_client_runtime(
    command: &ClientCommand,
    options: OpenClientRuntimeOptions,
) -> ChordResult<ClientRuntime> {
    validate(command)?;
    let directory = resolve_server_directory(options.directory.as_deref());
    let requested_server_id = options.requested_server_id.or_else(|| pi_ai::env::process_env_value(ENV_SERVER_ID));
    let session_dir = resolve_session_directory(options.session_dir.as_deref());
    let mut activated_client = None;
    let routes = match &command.connect {
        Some(TransportAddress::Radius { server_id }) => {
            vec![ClientRuntimeRoute::Radius { server_id: server_id.clone() }]
        }
        Some(TransportAddress::Unix { path }) => vec![ClientRuntimeRoute::Unix(route_from_explicit_path(path)?)],
        None => {
            let discovered = discover_unix_servers(DiscoverUnixServersOptions::new(&directory));
            let routes = discovered.await.map_err(ChordError::other)?;
            if !routes.is_empty() && command.model.is_some() {
                return Err(ChordError::error(
                    "Model selection is only valid when automatically activating a new server",
                ));
            }
            if routes.is_empty() {
                let activated = activate_server(ActivateServerOptions {
                    directory: directory.clone(),
                    requested_server_id: requested_server_id.clone(),
                    session_dir: session_dir.clone(),
                    provider: command.provider.clone(),
                    model: command.model.clone(),
                })
                .await?;
                activated_client = Some(activated.client);
                vec![ClientRuntimeRoute::Unix(activated.route)]
            } else {
                routes.into_iter().map(ClientRuntimeRoute::Unix).collect()
            }
        }
    };
    if command.plugin_packages.is_some() && routes.len() != 1 {
        return Err(ChordError::error("Plugin selection requires exactly one local server"));
    }
    let mut runtime = ClientRuntime::default();
    for route in routes {
        let client = match activated_client.take() {
            Some(client) => Ok(client),
            None => connect_route(command, &route, &directory, &session_dir).await,
        };
        let opened = match client {
            Ok(client) => runtime.add(route, client),
            Err(error) => Err(error),
        };
        if let Err(error) = opened {
            let cleanup = runtime.dispose().await.err().into_iter().collect();
            return Err(ChordError::with_cleanup(error, cleanup, "Experimental client startup and cleanup failed"));
        }
    }
    Ok(runtime)
}

/// Connect a route; a discovered Unix server with another protocol version is activated anew.
async fn connect_route(
    command: &ClientCommand,
    route: &ClientRuntimeRoute,
    directory: &Path,
    session_dir: &Path,
) -> ChordResult<Client> {
    let factory = match route {
        ClientRuntimeRoute::Unix(route) => {
            pi_client::unix::create_unix_transport_factory(UnixTransportOptions::new(&route.path))
                .map_err(ChordError::other)?
        }
        ClientRuntimeRoute::Radius { server_id } => {
            let auth = Arc::new(RadiusRelayAuthResolver::new(command.auth.clone(), None));
            create_radius_client_transport_factory(server_id.clone(), auth, None)
        }
    };
    let client = Client::new(ClientOptions::new(route.server_id(), factory)).map_err(ChordError::other)?;
    let Err(error) = client.connect().await else { return Ok(client) };
    client.dispose();
    if command.connect.is_some() || route.is_radius() || error.code() != Some("version") {
        return Err(ChordError::other(error));
    }
    let activated = activate_server(ActivateServerOptions {
        directory: directory.to_path_buf(),
        requested_server_id: Some(route.server_id().to_string()),
        session_dir: session_dir.to_path_buf(),
        provider: None,
        model: None,
    })
    .await?;
    Ok(activated.client)
}

pub(crate) fn open_services(
    source: &dyn RemoteServiceSource,
    services: &[&ServiceRef],
) -> ChordResult<Arc<dyn RemoteServices>> {
    source.open(RemoteServiceSourceOpenOptions {
        services: services.iter().map(|service| (*service).clone()).collect(),
        assert_access: Arc::new(|| Ok(())),
        on_error: Arc::new(|_| {}),
    })
}

impl ClientRuntime {
    fn add(&mut self, route: ClientRuntimeRoute, client: Client) -> ChordResult<()> {
        self.clients.push(client.clone());
        let server = ServerServiceSource::new(&client, None)?;
        self.server_sources.push(server.clone());
        if route.is_radius() {
            let services = open_services(server.as_ref(), &[&SESSION_MANAGEMENT])?;
            let management = SessionManagementClient(services.use_service(&SESSION_MANAGEMENT)?);
            let reconnect_client: Arc<dyn RadiusReconnectClient> = Arc::new(client.clone());
            self.reconnectors.push(RadiusClientReconnect::new(
                reconnect_client,
                Arc::new(move |session_id| {
                    let (services, management) = (services.clone(), management.clone());
                    Box::pin(async move {
                        services.ready(&BACKGROUND_CONTEXT).await?;
                        management.attach(session_id, &BACKGROUND_CONTEXT).await
                    })
                }),
            ));
        }
        let session = SessionServiceSource::new(&client, None)?;
        self.session_sources.push(session.clone());
        self.servers.push(ClientRuntimeServer { route, client, server, session });
        Ok(())
    }

    /// Stop reconnecting, dispose the service sources, then the clients.
    pub async fn dispose(&self) -> ChordResult<()> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        join_all(self.reconnectors.iter().map(RadiusClientReconnect::dispose)).await;
        let servers = join_all(self.server_sources.iter().map(|source| source.dispose(&BACKGROUND_CONTEXT)));
        let sessions = join_all(self.session_sources.iter().map(|source| source.dispose(&BACKGROUND_CONTEXT)));
        let (servers, sessions) = futures::join!(servers, sessions);
        let errors: Vec<ChordError> = servers.into_iter().chain(sessions).filter_map(Result::err).collect();
        for client in &self.clients {
            client.dispose();
        }
        ChordError::collect(errors, "Failed to dispose experimental client runtime")
    }
}

/// Upstream `SessionManagement` of an activated server: attach and detach also wait for the Session
/// services to follow.
#[derive(Clone)]
pub struct ActivatedSessionManagement {
    remote: SessionManagementClient,
    client: Client,
    session: Arc<SessionServiceSource>,
}

#[async_trait]
impl SessionManagement for ActivatedSessionManagement {
    async fn create(&self, options: SessionCreateOptions, context: &Context) -> ChordResult<SessionSummary> {
        self.remote.create(options, context).await
    }

    async fn remove(&self, session_id: String, context: &Context) -> ChordResult<()> {
        let removes_current = self.client.attachment().is_some_and(|target| target.session_id == session_id);
        self.remote.remove(session_id, context).await?;
        if removes_current {
            self.session.when_detached(context).await?;
        }
        Ok(())
    }

    async fn attach(&self, session_id: String, context: &Context) -> ChordResult<()> {
        self.remote.attach(session_id.clone(), context).await?;
        self.session.when_attached(&session_id, context).await
    }

    async fn detach(&self, context: &Context) -> ChordResult<()> {
        self.remote.detach(context).await?;
        self.session.when_detached(context).await
    }
}

/// Upstream `ActivatedClientRuntimeServer`.
pub struct ActivatedClientRuntimeServer {
    pub server: ClientRuntimeServer,
    pub directory: ServiceHandle,
    pub management: ActivatedSessionManagement,
    pub plugins: PresentationPluginsClient,
    pub models: ModelsClient,
    pub agent: AgentControllerClient,
    pub transcript: ServiceHandle,
}

/// Acquire and connect the built-in service facades used by the non-interactive client.
pub async fn activate_builtin_client_services(
    server: &ClientRuntimeServer,
) -> ChordResult<ActivatedClientRuntimeServer> {
    let server_services =
        open_services(server.server.as_ref(), &[&SESSION_DIRECTORY, &SESSION_MANAGEMENT, &PRESENTATION_PLUGINS])?;
    let session_services = open_services(server.session.as_ref(), &[&MODELS, &AGENT_CONTROLLER, &TRANSCRIPT])?;
    let management = ActivatedSessionManagement {
        remote: SessionManagementClient(server_services.use_service(&SESSION_MANAGEMENT)?),
        client: server.client.clone(),
        session: server.session.clone(),
    };
    let activated = ActivatedClientRuntimeServer {
        server: server.clone(),
        directory: server_services.use_service(&SESSION_DIRECTORY)?,
        management,
        plugins: PresentationPluginsClient(server_services.use_service(&PRESENTATION_PLUGINS)?),
        models: ModelsClient(session_services.use_service(&MODELS)?),
        agent: AgentControllerClient(session_services.use_service(&AGENT_CONTROLLER)?),
        transcript: session_services.use_service(&TRANSCRIPT)?,
    };
    let (first, second) =
        futures::join!(server_services.ready(&BACKGROUND_CONTEXT), session_services.ready(&BACKGROUND_CONTEXT));
    first.and(second)?;
    Ok(activated)
}
