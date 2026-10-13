//! Unix-domain socket transport and server discovery.

mod transport;

use std::io::ErrorKind;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use pi_protocol::is_server_id;

pub use transport::{UnixTransportOptions, create_unix_transport_factory};

use crate::client::Client;
use crate::errors::ClientError;
use crate::transport::ByteTransportFactory;
use crate::types::ClientOptions;

const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1_000);
const MAX_TIMER_DELAY_MS: u128 = 2_147_483_647;
const UNIX_SOCKET_SUFFIX: &str = ".sock";
const MAX_CONCURRENT_DISCOVERY_PROBES: usize = 16;

/// One reachable server and its socket path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixServerRoute {
    pub server_id: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct DiscoverUnixServersOptions {
    /// Directory containing server-addressed Unix sockets.
    pub directory: PathBuf,
    /// Maximum time for each connection and handshake. Defaults to one second.
    pub timeout: Option<Duration>,
}

impl DiscoverUnixServersOptions {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into(), timeout: None }
    }
}

/// Discover reachable local servers by probing `<serverId>.sock` entries in a directory.
///
/// Malformed names, non-sockets, stale or silent endpoints, and identity mismatches are skipped.
pub async fn discover_unix_servers(options: DiscoverUnixServersOptions) -> Result<Vec<UnixServerRoute>, ClientError> {
    let timeout = options.timeout.unwrap_or(DEFAULT_DISCOVERY_TIMEOUT);
    if timeout.is_zero() || timeout.as_millis() > MAX_TIMER_DELAY_MS {
        let message = format!("Unix discovery timeoutMs must be an integer between 1 and {MAX_TIMER_DELAY_MS}");
        return Err(ClientError::Type(message));
    }
    let entries = match std::fs::read_dir(&options.directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(server_id) = name.strip_suffix(UNIX_SOCKET_SUFFIX) else { continue };
        if is_server_id(server_id) {
            candidates.push(UnixServerRoute { server_id: server_id.into(), path: options.directory.join(name) });
        }
    }
    let probes = futures::stream::iter(candidates)
        .map(|candidate| probe_candidate(candidate, timeout))
        .buffer_unordered(MAX_CONCURRENT_DISCOVERY_PROBES);
    let results: Vec<_> = probes.collect().await;
    let mut routes = Vec::new();
    for result in results {
        if let Some(route) = result? {
            routes.push(route);
        }
    }
    routes.sort_by(|left, right| left.server_id.cmp(&right.server_id));
    Ok(routes)
}

async fn probe_candidate(
    candidate: UnixServerRoute,
    timeout: Duration,
) -> Result<Option<UnixServerRoute>, ClientError> {
    match std::fs::symlink_metadata(&candidate.path) {
        // A socket can disappear between listing and probing during normal server shutdown.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.file_type().is_socket() => return Ok(None),
        Ok(_) => {}
    }
    probe_unix_server(candidate, timeout).await
}

fn probe_factory(path: &Path) -> ByteTransportFactory {
    let path = Arc::new(path.to_path_buf());
    Arc::new(move |handlers| {
        let path = path.clone();
        Box::pin(async move { transport::connect_unix_socket(&path, DEFAULT_PENDING_BYTES, handlers).await })
    })
}

const DEFAULT_PENDING_BYTES: usize = pi_protocol::DEFAULT_MAX_FRAME_LENGTH as usize * 4;

async fn probe_unix_server(route: UnixServerRoute, timeout: Duration) -> Result<Option<UnixServerRoute>, ClientError> {
    let client = Client::new(ClientOptions::new(route.server_id.clone(), probe_factory(&route.path)))?;
    let outcome = tokio::time::timeout(timeout, client.connect()).await;
    client.dispose();
    let error = match outcome {
        Err(_) => return Ok(None),
        Ok(Ok(_)) => return Ok(Some(route)),
        Ok(Err(error)) => error,
    };
    let ignorable_io = matches!(
        error.io_kind(),
        Some(
            ErrorKind::NotFound
                | ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::BrokenPipe
                | ErrorKind::TimedOut
        )
    );
    // Stale sockets and endpoints that are not the advertised server are safe to omit.
    let ignorable = ignorable_io
        || matches!(error, ClientError::Validation(_) | ClientError::Disconnected { cause: None, .. })
        || error.code() == Some("version");
    if ignorable { Ok(None) } else { Err(error) }
}
