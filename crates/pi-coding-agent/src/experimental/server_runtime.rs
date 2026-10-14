//! Start a replaceable experimental server behind the stable coordinator endpoint (upstream `startServer`,
//! `startForegroundServer`, and `runServerProcess`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult};
use pi_server::Server;
use pi_server::transports::unix::{UnixServerOptions, create_unix_server, get_unix_socket_path};

use super::control::signals;
use super::coordinator::{CoordinatorConnection, WorkerCoordinator, ensure_coordinator};
use super::plugins::package::restore_server_plugin_package_profile;
use super::radius_auth::RadiusRelayAuthResolver;
use super::radius_relay::radius_relay_host_factory;
use super::relay::{NoRelayHost, RelayHost, RelayHostFactory, RelayHostOptions, RelayStatusListener};
use super::server::{
    ENV_SERVER_ID, ServerLifetime, acquire_server_activation, acquire_server_profile, ensure_private_server_directory,
    resolve_server_directory, resolve_session_directory,
};
use super::server_backend::{ExperimentalServerHost, PluginSelections, RoutingHost};
use super::services::server::create_experimental_server_services;
use super::session_worker_manager::{SessionWorkerManager, WorkerModel};

#[derive(Clone, Default)]
pub struct StartServerOptions {
    /// Server profile and socket directory. Defaults to `PI_SERVER_DIR` or `~/.pi/server`.
    pub directory: Option<String>,
    /// Logical service ID. Defaults to `PI_SERVER_ID` or the directory's `default-server-id`.
    pub server_id: Option<String>,
    /// Durable Session directory. Defaults to `experimental/sessions` under the agent directory.
    pub session_dir: Option<String>,
    pub provider: Option<String>,
    /// Model override for newly started Session workers.
    pub model: Option<String>,
    /// Hold the server open without client or Session demand. Defaults to true.
    pub keep_alive: Option<bool>,
    /// Explicit plugin packages. `None` restores the server profile; an empty list clears it.
    pub plugin_packages: Option<Vec<String>>,
    /// The relay host, such as `radius_relay::radius_relay_host_factory`. `None` runs local only.
    pub relay: Option<RelayHostFactory>,
    pub on_relay_status: Option<RelayStatusListener>,
}

type SharedClose = Shared<BoxFuture<'static, Result<(), ChordError>>>;

/// A running server generation.
pub struct RunningServer {
    pub server_id: String,
    pub session_dir: PathBuf,
    pub socket_path: PathBuf,
    pub server: Arc<Server<RoutingHost>>,
    workers: SessionWorkerManager,
    closed: SharedClose,
    close: Arc<OnceLock<SharedClose>>,
    shutdown: Shutdown,
}

type Shutdown = Arc<dyn Fn() -> BoxFuture<'static, ChordResult<()>> + Send + Sync>;

fn close_once(close: &OnceLock<SharedClose>, shutdown: &Shutdown) -> SharedClose {
    close.get_or_init(|| shutdown().shared()).clone()
}

impl RunningServer {
    /// Worker process IDs by Session ID.
    pub fn worker_pids(&self) -> HashMap<String, u32> {
        self.workers.worker_pids()
    }

    /// Resolves after the server shut down.
    pub async fn closed(&self) -> ChordResult<()> {
        self.closed.clone().await
    }

    pub async fn close(&self) -> ChordResult<()> {
        close_once(&self.close, &self.shutdown).await
    }
}

fn worker_model(options: &StartServerOptions) -> ChordResult<Option<WorkerModel>> {
    match (&options.provider, &options.model) {
        (Some(_), None) => Err(ChordError::error("Server model provider requires a model")),
        (provider, Some(model)) => Ok(Some(WorkerModel { provider: provider.clone(), model: model.clone() })),
        (None, None) => Ok(None),
    }
}

/// Start a replaceable experimental server behind the stable coordinator endpoint.
pub async fn start_server(options: StartServerOptions) -> ChordResult<RunningServer> {
    let model = worker_model(&options)?;
    let directory = resolve_server_directory(options.directory.as_deref());
    let requested = options.server_id.clone().or_else(|| std::env::var(ENV_SERVER_ID).ok().filter(|id| !id.is_empty()));
    let profile = acquire_server_profile(&directory, requested.as_deref()).await?;
    let server_id = profile.server_id.clone();
    let lifetime = ServerLifetime::new(options.keep_alive.unwrap_or(true));
    let started = start(&options, model, &directory, &server_id, &lifetime).await;
    if started.is_err() {
        lifetime.stop();
    }
    let released = profile.release().await;
    let running = started?;
    released?;
    Ok(running)
}

async fn start(
    options: &StartServerOptions,
    model: Option<WorkerModel>,
    directory: &Path,
    server_id: &str,
    lifetime: &ServerLifetime,
) -> ChordResult<RunningServer> {
    ensure_private_server_directory(directory).await?;
    let packages =
        restore_server_plugin_package_profile(directory, server_id, options.plugin_packages.as_deref()).await?;
    let plugins = PluginSelections::new(directory, server_id, &packages).await?;
    let socket_path = get_unix_socket_path(server_id, directory)?;
    let control_path = directory.join(format!("control-{server_id}.sock"));
    let nonce: String = pi_server::random_uuid().replace('-', "").chars().take(12).collect();
    let server_path = directory.join(format!("server-{server_id}-{nonce}.sock"));
    let lease = ensure_coordinator(&socket_path, &control_path).await?;
    let coordinator = CoordinatorConnection::new(control_path, server_path.display().to_string(), None);
    let session_dir = resolve_session_directory(options.session_dir.as_deref());
    let worker_lifetime = lifetime.clone();
    let workers = SessionWorkerManager::new(
        Arc::new(coordinator.clone()),
        session_dir.display().to_string(),
        model,
        Some(Arc::new(move |count| worker_lifetime.set_worker_count(count))),
    );
    let _ = plugins.workers.set(workers.clone());
    let host = Arc::new(ExperimentalServerHost {
        server_id: server_id.to_string(),
        session_dir: session_dir.clone(),
        workers: workers.clone(),
        plugins,
        services: OnceLock::new(),
    });
    let fail = |error: ChordError, workers: &SessionWorkerManager| {
        let (workers, coordinator) = (workers.clone(), coordinator.clone());
        async move {
            if coordinator.was_replaced() {
                workers.detach();
            } else {
                workers.shutdown().await;
            }
            coordinator.close();
            Err(error)
        }
    };
    let services = match create_experimental_server_services(host.clone()).await {
        Ok(services) => Arc::new(services),
        Err(error) => return fail(error, &workers).await,
    };
    let _ = host.services.set(services.host());
    let mut unix_options = UnixServerOptions::new(server_id, &server_path);
    unix_options.mode = Some(0o600);
    let connection_lifetime = lifetime.clone();
    unix_options.on_connection_count_changed =
        Some(Arc::new(move |count| connection_lifetime.set_connection_count(count)));
    let server = Arc::new(create_unix_server(Arc::new(RoutingHost(host.clone())), unix_options)?);
    let startup = async {
        server.start().await?;
        coordinator.connect().await?;
        drop(lease);
        workers.discover(&coordinator.peer_ids()).await;
        services.refresh(&BACKGROUND_CONTEXT).await
    };
    if let Err(error) = startup.await {
        let _ = server.close().await;
        let _ = services.dispose().await;
        return fail(error, &workers).await;
    }
    let relay: Arc<dyn RelayHost> = match &options.relay {
        Some(factory) => factory(RelayHostOptions {
            server_id: server_id.to_string(),
            socket_path: socket_path.clone(),
            accept: {
                let server = server.clone();
                Arc::new(move |connection| server.accept(connection))
            },
            on_status: options.on_relay_status.clone(),
        }),
        None => Arc::new(NoRelayHost),
    };
    relay.start();
    let (closed_server, closed_services, closed_relay) = (server.clone(), services.clone(), relay.clone());
    let closed: SharedClose = async move {
        let result = closed_server.closed().await;
        let disposed = closed_services.dispose().await;
        let _ = closed_relay.close().await;
        match (result, disposed) {
            (Err(error), Err(cleanup)) => {
                Err(ChordError::with_cleanup(error, vec![cleanup], "Server and repository shutdown failed"))
            }
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
    .boxed()
    .shared();
    tokio::spawn(closed.clone());
    let (replaced_lifetime, replaced_workers, replaced_relay) = (lifetime.clone(), workers.clone(), relay.clone());
    let (replaced_server, replaced_coordinator) = (server.clone(), coordinator.clone());
    tokio::spawn(async move {
        replaced_coordinator.replaced().await;
        replaced_lifetime.stop();
        replaced_workers.detach();
        let _ = replaced_relay.close().await;
        let _ = replaced_server.close().await;
        replaced_coordinator.close();
    });
    let (stop_lifetime, stop_closed) = (lifetime.clone(), closed.clone());
    let (stop_server, stop_workers, stop_coordinator) = (server.clone(), workers.clone(), coordinator.clone());
    let shutdown: Shutdown = Arc::new(move || {
        stop_lifetime.stop();
        let (relay, server, closed) = (relay.clone(), stop_server.clone(), stop_closed.clone());
        let (workers, coordinator) = (stop_workers.clone(), stop_coordinator.clone());
        async move {
            let _ = relay.close().await;
            let _ = server.close().await;
            let result = closed.await;
            if coordinator.was_replaced() {
                workers.detach();
            } else {
                workers.shutdown().await;
            }
            coordinator.close();
            result
        }
        .boxed()
    });
    let running = RunningServer {
        server_id: server_id.to_string(),
        session_dir,
        socket_path,
        server,
        workers,
        closed,
        close: Arc::default(),
        shutdown,
    };
    let (retire_close, retire_shutdown) = (running.close.clone(), running.shutdown.clone());
    lifetime.start(Arc::new(move || {
        tokio::spawn(close_once(&retire_close, &retire_shutdown));
    }));
    Ok(running)
}

/// Start an operator-held server while serializing against automatic cold activation.
pub async fn start_foreground_server(mut options: StartServerOptions) -> ChordResult<RunningServer> {
    let directory = resolve_server_directory(options.directory.as_deref());
    ensure_private_server_directory(&directory).await?;
    let requested = options.server_id.clone().or_else(|| std::env::var(ENV_SERVER_ID).ok().filter(|id| !id.is_empty()));
    let profile = acquire_server_profile(&directory, requested.as_deref()).await?;
    let server_id = profile.server_id.clone();
    profile.release().await?;
    let activation = acquire_server_activation(&directory, &server_id).await?;
    options.directory = Some(directory.display().to_string());
    options.server_id = Some(server_id);
    options.keep_alive = Some(true);
    let started = start_server(options).await;
    let released = activation.release().await;
    let running = started?;
    released?;
    Ok(running)
}

fn parse_server_model_options(value: Option<&String>) -> ChordResult<Option<(Option<String>, String)>> {
    let Some(value) = value else { return Ok(None) };
    let invalid = || ChordError::error("Internal server received invalid model options");
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|_| invalid())?;
    let object = parsed.as_object().ok_or_else(invalid)?;
    let text =
        |key: &str| object.get(key).map(|value| value.as_str().filter(|text| !text.is_empty()).map(str::to_string));
    let (model, provider) = (text("model"), text("provider"));
    if object.keys().any(|key| key != "provider" && key != "model")
        || !matches!(model, Some(Some(_)))
        || matches!(provider, Some(None))
    {
        return Err(invalid());
    }
    Ok(Some((provider.flatten(), model.flatten().unwrap_or_default())))
}

/// Run an automatically activated server until its client and Session demand disappears.
pub async fn run_server_process(args: &[String]) -> ChordResult<()> {
    if args.len() > 4 {
        return Err(ChordError::error("Internal server received unexpected arguments"));
    }
    let directory = args.first().filter(|path| Path::new(path).is_absolute());
    let directory =
        directory.ok_or_else(|| ChordError::error("Internal server requires an absolute server directory"))?;
    let server_id = args.get(1).filter(|id| pi_protocol::is_server_id(id));
    let server_id = server_id.ok_or_else(|| ChordError::error("Internal server requires a canonical server ID"))?;
    let session_dir = args.get(2).filter(|path| Path::new(path).is_absolute());
    let session_dir =
        session_dir.ok_or_else(|| ChordError::error("Internal server requires an absolute Session directory"))?;
    let model = parse_server_model_options(args.get(3))?;
    let runtime = start_server(StartServerOptions {
        directory: Some(directory.clone()),
        server_id: Some(server_id.clone()),
        session_dir: Some(session_dir.clone()),
        keep_alive: Some(false),
        provider: model.as_ref().and_then(|(provider, _)| provider.clone()),
        model: model.map(|(_, model)| model),
        relay: Some(radius_relay_host_factory(Arc::new(RadiusRelayAuthResolver::new(None, None)), None)),
        ..StartServerOptions::default()
    })
    .await?;
    let mut signals = signals()?;
    tokio::select! {
        _ = runtime.closed() => {}
        () = signals.recv() => {}
    }
    let result = runtime.close().await;
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
