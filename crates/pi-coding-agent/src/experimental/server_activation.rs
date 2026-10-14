//! Automatic server activation (upstream `activateServer` in `server.ts`).

use std::path::PathBuf;
use std::time::Duration;

use pi_chord::{ChordError, ChordResult};
use pi_client::unix::{UnixServerRoute, UnixTransportOptions, create_unix_transport_factory};
use pi_client::{Client, ClientError, ClientOptions};

use super::process::{
    InternalProcessRole, InternalProcessSpawnOptions, spawn_internal_process, terminate_internal_process,
};
use super::server::{
    ACTIVATION_TIMEOUT, acquire_server_activation, acquire_server_profile, ensure_private_server_directory,
};

const ACTIVATION_RETRY: Duration = Duration::from_millis(10);

pub struct ActivatedServer {
    pub client: Client,
    pub route: UnixServerRoute,
}

pub struct ActivateServerOptions {
    pub directory: PathBuf,
    pub requested_server_id: Option<String>,
    pub session_dir: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Connect to the selected logical server, launching the current Pi installation when none answers.
pub async fn activate_server(options: ActivateServerOptions) -> ChordResult<ActivatedServer> {
    if options.provider.is_some() && options.model.is_none() {
        return Err(ChordError::error("Server model provider requires a model"));
    }
    ensure_private_server_directory(&options.directory).await?;
    let profile = acquire_server_profile(&options.directory, options.requested_server_id.as_deref()).await?;
    let server_id = profile.server_id.clone();
    profile.release().await?;
    let path = pi_server::transports::unix::get_unix_socket_path(&server_id, &options.directory)?;
    let route = UnixServerRoute { server_id: server_id.clone(), path };
    let activation = acquire_server_activation(&options.directory, &server_id).await?;
    let result = activate(&options, &route).await;
    let released = activation.release().await;
    let client = result?;
    released?;
    Ok(ActivatedServer { client, route })
}

async fn activate(options: &ActivateServerOptions, route: &UnixServerRoute) -> ChordResult<Client> {
    if let Some(existing) = connect(route).await? {
        // Another activator won the race, so startup-only selections can no longer be applied.
        if options.model.is_some() {
            existing.dispose();
            return Err(ChordError::error("Model selection is only valid when automatically activating a new server"));
        }
        return Ok(existing);
    }
    let mut args = vec![
        options.directory.display().to_string(),
        route.server_id.clone(),
        options.session_dir.display().to_string(),
    ];
    if let Some(model) = &options.model {
        let mut value = serde_json::Map::new();
        if let Some(provider) = &options.provider {
            value.insert("provider".into(), provider.clone().into());
        }
        value.insert("model".into(), model.clone().into());
        args.push(serde_json::Value::Object(value).to_string());
    }
    let child = spawn_internal_process(InternalProcessRole::Server, &args, InternalProcessSpawnOptions::default())
        .map_err(|error| ChordError::error(format!("Failed to automatically activate server: {error}")))?;
    let deadline = tokio::time::Instant::now() + ACTIVATION_TIMEOUT;
    let waited = async {
        loop {
            if let Some(client) = connect(route).await? {
                return Ok(client);
            }
            if child.has_exited() {
                return Err(ChordError::error("Automatically activated server exited during startup"));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ChordError::error("Timed out waiting for automatically activated server"));
            }
            tokio::time::sleep(ACTIVATION_RETRY).await;
        }
    };
    let result = waited.await;
    if result.is_err() {
        terminate_internal_process(&child).await;
    }
    result
}

/// Connect to a route, or `None` when no compatible server answers there.
pub async fn connect(route: &UnixServerRoute) -> ChordResult<Option<Client>> {
    let factory = create_unix_transport_factory(UnixTransportOptions::new(&route.path)).map_err(ChordError::other)?;
    let client = Client::new(ClientOptions::new(&route.server_id, factory)).map_err(ChordError::other)?;
    match client.connect().await {
        Ok(_) => Ok(Some(client)),
        Err(error) => {
            client.dispose();
            if absent(&error) { Ok(None) } else { Err(ChordError::other(error)) }
        }
    }
}

fn absent(error: &ClientError) -> bool {
    use std::io::ErrorKind;
    if matches!(error, ClientError::Disconnected { .. }) || error.code() == Some("version") {
        return true;
    }
    let kinds = [
        ErrorKind::NotFound,
        ErrorKind::ConnectionRefused,
        ErrorKind::ConnectionReset,
        ErrorKind::BrokenPipe,
        ErrorKind::TimedOut,
    ];
    error.io_kind().is_some_and(|kind| kinds.contains(&kind))
}
