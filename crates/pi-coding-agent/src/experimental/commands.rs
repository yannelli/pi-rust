//! Development-only experimental command handlers (upstream `experimental/commands.ts`).
//! TODO(step 3): `runClientCommand` and `runExperimentalCommand` need client.ts, client-tui.ts, and the
//! `cli/experimental` parser; step 3 adds them and builds [`ServerCommand`] from parsed options.

use std::sync::{Arc, Mutex};

use pi_chord::ChordResult;

use super::control::signals;
use super::process::lock;
use super::radius_auth::{RadiusAuthInput, RadiusRelayAuthResolver};
use super::radius_relay::radius_relay_host_factory;
use super::relay::RadiusRelayHostStatus;
use super::server_runtime::{StartServerOptions, start_foreground_server};

/// Options of `pi server`.
#[derive(Clone, Debug, Default)]
pub struct ServerCommand {
    pub server_id: Option<String>,
    pub session_dir: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plugin_packages: Option<Vec<String>>,
    /// Explicit Radius credential. Stored Radius auth is used when omitted.
    pub auth: Option<RadiusAuthInput>,
}

fn describe(status: &RadiusRelayHostStatus) -> Option<String> {
    match status {
        RadiusRelayHostStatus::Connecting => None,
        RadiusRelayHostStatus::Connected => Some("connected".into()),
        RadiusRelayHostStatus::NotAuthenticated => Some("not connected; local only".into()),
        RadiusRelayHostStatus::Retrying { error } => Some(format!("reconnecting: {error}")),
    }
}

#[derive(Default)]
struct RelayOutput {
    previous: String,
    ready: bool,
    pending: Option<RadiusRelayHostStatus>,
}

fn report(output: &mut RelayOutput, status: &RadiusRelayHostStatus) {
    let Some(description) = describe(status) else { return };
    if description != output.previous {
        println!("Radius: {description}");
        output.previous = description;
    }
}

/// Run a foreground server until SIGINT, SIGTERM, or shutdown.
pub async fn run_server_command(command: ServerCommand) -> ChordResult<()> {
    let output = Arc::new(Mutex::new(RelayOutput::default()));
    let status_output = output.clone();
    let runtime = start_foreground_server(StartServerOptions {
        server_id: command.server_id,
        session_dir: command.session_dir,
        provider: command.provider,
        model: command.model,
        plugin_packages: Some(command.plugin_packages.unwrap_or_default()),
        relay: Some(radius_relay_host_factory(Arc::new(RadiusRelayAuthResolver::new(command.auth, None)), None)),
        on_relay_status: Some(Arc::new(move |status| {
            let mut output = lock(&status_output);
            if output.ready {
                report(&mut output, status);
            } else {
                output.pending = Some(status.clone());
            }
        })),
        ..StartServerOptions::default()
    })
    .await?;
    println!("Server: {}", runtime.server_id);
    println!("Socket: {}", runtime.socket_path.display());
    {
        let mut output = lock(&output);
        output.ready = true;
        if let Some(pending) = output.pending.take() {
            report(&mut output, &pending);
        }
    }
    let mut signals = signals()?;
    let finished = tokio::select! {
        result = runtime.closed() => result,
        () = signals.recv() => Ok(()),
    };
    let closed = runtime.close().await;
    finished.and(closed)
}
