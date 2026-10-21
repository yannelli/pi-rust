//! The parts that wait outside the connection task: resolving a route, and a call through a relay.

use tokio::sync::oneshot;

use super::Route;
use super::net::{ConnId, Msg, RelayId, RelayedCall, WeakNet};
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::{ChordErrorCode, ErrorOptions};
use crate::host2::wire::Payload;
use crate::json::Json;
use crate::sync::BoxFuture;

fn task_gone() -> ChordError {
    ChordError::code_only(ChordErrorCode::Unavailable, "host closed")
}

/// Authorize, resolve (maybe spawn), authorize again; the connection to the host once it is usable. `still`
/// says whether the request is still authorized as it was (a revocation during resolution wins).
pub(super) async fn resolve_route(
    net: &WeakNet,
    route: &Route,
    host_name: &str,
    principal: &Json,
    still: impl Fn() -> bool,
) -> ChordResult<ConnId> {
    let refuse = || ChordError::code_only(ChordErrorCode::Forbidden, format!("{host_name} refused for this principal"));
    let revoked = || ChordError::code_only(ChordErrorCode::Gone, "principal invalidated while resolving");
    if !(route.authorize)(principal, host_name).await {
        return Err(refuse());
    }
    if !still() {
        return Err(revoked());
    }
    let link = (route.resolve)(host_name).await?;
    if !still() {
        return Err(revoked());
    }
    if !(route.authorize)(principal, host_name).await {
        return Err(refuse());
    }
    if !still() {
        return Err(revoked());
    }
    let (reply, attached) = oneshot::channel();
    let principal = (route.principal)(host_name);
    if !net.post(Msg::Attach { link, name: host_name.to_string(), principal, reply }) {
        return Err(task_gone());
    }
    let conn = attached.await.unwrap_or_else(|_| Err(task_gone()))?;
    if !still() {
        return Err(revoked());
    }
    Ok(conn)
}

/// A call through a relay: waits for the route, then forwards with the root it runs in. Settles exactly once; a
/// cancel (or the origin going away) is forwarded downstream.
pub(super) fn relay_call(net: WeakNet, relay: RelayId, call: RelayedCall) -> BoxFuture<ChordResult<Option<Payload>>> {
    Box::pin(async move {
        if let Some(reason) = call.signal.reason() {
            let options = ErrorOptions::default().with_cause(reason);
            return Err(ChordError::coded(ChordErrorCode::Aborted, "call cancelled", options));
        }
        let (reply, result) = oneshot::channel();
        if !net.post(Msg::RelayCall { relay, call, reply }) {
            return Err(task_gone());
        }
        result.await.unwrap_or_else(|_| Err(ChordError::code_only(ChordErrorCode::Gone, "relayed hold closed")))
    })
}
