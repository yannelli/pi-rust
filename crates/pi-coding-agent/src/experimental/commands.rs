//! Development-only experimental command handlers (upstream `experimental/commands.ts`).

use std::sync::{Arc, Mutex};

use pi_chord::ChordResult;

use super::cli::{ClientCommand, ExperimentalCommand, parse};
use super::client::{ClientResult, run_client};
use super::client_tui_run::run_client_tui;
use super::control::signals;
use super::process::lock;
use super::radius_auth::RadiusRelayAuthResolver;
use super::radius_relay::radius_relay_host_factory;
use super::relay::RadiusRelayHostStatus;
use super::server_runtime::{StartServerOptions, start_foreground_server};

pub use super::cli::ServerCommand;

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

/// Open the TUI on a terminal without a prompt; otherwise list, attach, or print the answer.
pub async fn run_client_command(command: ClientCommand) -> ChordResult<()> {
    use std::io::IsTerminal;
    if command.prompt.is_none() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return run_client_tui(&command, Default::default()).await;
    }
    match run_client(&command, Default::default()).await? {
        ClientResult::Attached { server_id, session_id } => println!("{server_id}\t{session_id}\tattached"),
        ClientResult::Prompted { text, .. } => println!("{text}"),
        ClientResult::List { sessions } => {
            for session in sessions {
                println!("{}\t{}", session.server_id, session.session_id);
            }
        }
    }
    Ok(())
}

fn print_error(error: &str) {
    use std::io::IsTerminal;
    match std::io::stderr().is_terminal() {
        true => eprintln!("\x1b[31mError: {error}\x1b[39m"),
        false => eprintln!("Error: {error}"),
    }
}

/// Upstream `runExperimentalCommand` for the arguments after `pi experimental`; returns the exit code.
pub async fn run_experimental_command(args: &[String]) -> i32 {
    let result = match parse(args) {
        Ok(ExperimentalCommand::Server(command)) => run_server_command(command).await,
        Ok(ExperimentalCommand::Client(command)) => run_client_command(command).await,
        Err(errors) => {
            for error in errors {
                print_error(&error);
            }
            return 1;
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            print_error(&error.to_string());
            1
        }
    }
}
