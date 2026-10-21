//! The coordinating server's side of a cluster reload (upstream `runClusterReload`): prepare here and on every
//! child host in step, collect the votes, then commit everywhere or abort everywhere. Links are FIFO, so a child
//! commits before any frame of a root the new code version started reaches it.

use std::sync::Arc;
use std::time::Duration;

use futures::future::{join, join_all};
use indexmap::IndexMap;
use tokio::sync::oneshot;

use super::{DEFAULT_DEADLINE, Reload, ReloadOptions, ReloadReport, load, reload_error};
use crate::error::{ChordError, ChordResult};
use crate::host2::connection::{Participant, VoteResult, next_reload_id};
use crate::host2::errors::{ChordErrorCode, ErrorOptions};
use crate::host2::host::{HostImpl, PluginRef, modules_for};
use crate::host2::wire::{Prepare, WireReloadOptions, WireReloadReport};

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// A child's vote, or why it is missing; a failure names the child.
async fn vote(name: String, vote: oneshot::Receiver<VoteResult>, deadline: Duration) -> VoteOutcome {
    let result = match tokio::time::timeout(deadline, vote).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(ChordError::code_only(ChordErrorCode::Unavailable, "link to child closed")),
        Err(_) => {
            let message = format!("votes missing after {} ms", deadline.as_millis());
            Err(ChordError::with_reason(ChordErrorCode::Aborted, "vote_deadline", message))
        }
    };
    let error = match result {
        Ok(report) => return Ok((name, report)),
        Err(error) => reload_error(error),
    };
    let code = error.code().unwrap_or(ChordErrorCode::Unavailable);
    let options =
        ErrorOptions { reason: error.reason().map(str::to_string), details: None, cause: Some(error.clone()) };
    Err(ChordError::coded(code, format!("{name}: {error}"), options))
}

type VoteOutcome = ChordResult<(String, Option<WireReloadReport>)>;

/// Runs a cluster reload that took the host's reload slot, with the children in step when it began.
pub(super) async fn run_cluster_reload(
    host: Arc<HostImpl>,
    plugins: Vec<PluginRef>,
    options: ReloadOptions,
    synced: Vec<Participant>,
) -> ChordResult<ReloadReport> {
    let code_version = host.peek_next_code_version();
    let reload = load(&host, plugins, options.clone(), Some(code_version)).await?;
    let result = coordinate(&host, &reload, &options, synced, code_version).await;
    host.reload_ended();
    result
}

async fn coordinate(
    host: &Arc<HostImpl>,
    reload: &Arc<Reload>,
    options: &ReloadOptions,
    synced: Vec<Participant>,
    code_version: i64,
) -> ChordResult<ReloadReport> {
    // Retire first: its `retire` frames reach every child before `prepare`, so a child never retires a code
    // version this host still runs roots of.
    reload.wait_for_retirement().await;
    let net = host.net_if_started().cloned();
    let participants = match &net {
        Some(net) => net.still_open(synced).await,
        None => Vec::new(),
    };
    let id = next_reload_id();
    let deadline =
        options.vote_deadline.unwrap_or(options.gate_deadline() + options.retire_deadline() + DEFAULT_DEADLINE);
    let frames = participants.iter().map(|participant| {
        let remove = modules_for(options.remote_remove.as_ref(), &participant.name);
        let wire = WireReloadOptions {
            remove: Some(remove),
            gate_deadline: options.gate_deadline.map(millis),
            retire_deadline: options.retire_deadline.map(millis),
        };
        let plugins = modules_for(options.remote.as_ref(), &participant.name);
        (participant.conn, Prepare { reload: id, code_version: code_version as u64, plugins, options: wire })
    });
    let receivers = match &net {
        Some(net) => net.prepare(frames.collect()),
        None => Vec::new(),
    };
    let votes = participants
        .iter()
        .zip(receivers)
        .map(|(participant, receiver)| vote(participant.name.clone(), receiver, deadline));
    let (local, votes) = join(reload.prepare(), join_all(votes)).await;
    let conns: Vec<_> = participants.iter().map(|participant| participant.conn).collect();
    let failed = local.as_ref().err().cloned().or_else(|| votes.iter().find_map(|vote| vote.as_ref().err().cloned()));
    if let Some(failed) = failed {
        if let Some(net) = &net {
            net.abort(conns, id);
        }
        if local.is_ok() {
            reload.rollback().await;
        }
        return Err(failed);
    }
    let before_start: Box<dyn FnOnce() + Send> = Box::new(move || {
        if let Some(net) = net {
            net.commit(conns, id, code_version as u64);
        }
    });
    let mut report = reload.commit(Some(before_start))?;
    host.remember_remote_modules(options.remote.as_ref(), options.remote_remove.as_ref());
    let mut children = IndexMap::new();
    for (name, child) in votes.into_iter().flatten() {
        if let Some(child) = child {
            children.insert(name, ReloadReport::from_wire(child));
        }
    }
    report.children = children;
    Ok(report)
}
