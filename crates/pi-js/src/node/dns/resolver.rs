//! DNS record queries use UDP, TCP fallback, and bounded CNAME following. Each query starts at the first server.

#[cfg(test)]
mod tests;
mod transport;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

use super::wire::{Kind, Question, Reply};
use transport::Attempt;

/// Alias hops followed before the query fails with `ENODATA`.
const MAX_ALIASES: usize = 16;
/// Caps the per-round timeout doubling so the shift cannot overflow.
const MAX_ROUNDS: u32 = 16;
/// c-ares `MIN_TIMEOUT_MS` and `MAX_TIMEOUT_MS` from `ares_metrics.c`.
const MIN_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_TIMEOUT: Duration = Duration::from_millis(5000);

pub(crate) const CANCELLED: &str = "ECANCELLED";
pub(crate) const CONNREFUSED: &str = "ECONNREFUSED";
pub(crate) const TIMEOUT: &str = "ETIMEOUT";
pub(crate) const BADRESP: &str = "EBADRESP";
pub(crate) const NODATA: &str = "ENODATA";
/// Node 22 reports this when the server list is empty.
pub(crate) const NO_SERVERS: &str = "UNKNOWN_ARES_ERROR";

pub(crate) struct Resolver {
    servers: Vec<SocketAddr>,
    timeout: Duration,
    tries: usize,
    max_timeout: Option<Duration>,
}

/// Node sets `ARES_FLAG_NOCHECKRESP`, so every rcode ends the query; only transport
/// failures and unparsable replies move on to the next attempt.
fn retryable(code: &str) -> bool {
    matches!(code, TIMEOUT | CONNREFUSED | BADRESP)
}

fn random_u16() -> Result<u16, &'static str> {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes).map_err(|_| "ENOMEM")?;
    Ok(u16::from_ne_bytes(bytes))
}

fn name_key(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

impl Resolver {
    pub(crate) fn new(
        servers: Vec<SocketAddr>,
        timeout: Duration,
        tries: usize,
        max_timeout: Option<Duration>,
    ) -> Self {
        Self { servers, timeout, tries: tries.max(1), max_timeout }
    }

    pub(super) fn with_servers(&self, servers: Vec<SocketAddr>) -> Self {
        Self { servers, ..*self }
    }

    /// Wait for one attempt, from c-ares `ares_calc_query_timeout` for a server with no metrics:
    /// each round over the servers doubles the wait, then jitter takes off up to half.
    fn attempt_wait(&self, round: u32) -> Result<Duration, &'static str> {
        let base = self.timeout.max(MIN_TIMEOUT).min(self.max_timeout.unwrap_or(MAX_TIMEOUT));
        if round == 0 {
            return Ok(base);
        }
        let mut wait = base.saturating_mul(1 << round.min(MAX_ROUNDS));
        if let Some(max) = self.max_timeout {
            wait = wait.min(max);
        }
        let jitter = f64::from(random_u16()?) / f64::from(u16::MAX) * 0.5;
        Ok(wait.mul_f64(1.0 - jitter).max(base))
    }

    /// Resolves `name`, following aliases. Cancelling `cancel` drops every open socket at once.
    pub(crate) async fn resolve(
        &self,
        name: &str,
        kind: Kind,
        ttl: bool,
        local: (Ipv4Addr, Ipv6Addr),
        cancel: CancellationToken,
    ) -> Result<Json, &'static str> {
        if cancel.is_cancelled() {
            return Err(CANCELLED);
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CANCELLED),
            result = self.follow(name, kind, ttl, local) => result,
        }
    }

    async fn follow(
        &self,
        name: &str,
        kind: Kind,
        ttl: bool,
        local: (Ipv4Addr, Ipv6Addr),
    ) -> Result<Json, &'static str> {
        let mut question = Question::new(name, kind)?;
        let mut seen = Vec::new();
        for _ in 0..=MAX_ALIASES {
            match self.query(&question, ttl, local).await? {
                Reply::Answer(value) => return Ok(value),
                Reply::Alias { name: target, ttl } => {
                    if seen.is_empty() {
                        seen.push(name_key(name));
                    }
                    let key = name_key(&target);
                    if seen.contains(&key) {
                        return Err(NODATA);
                    }
                    seen.push(key);
                    question = Question::new(&target, kind)?.with_ttl_cap(ttl);
                }
                Reply::Truncated | Reply::Unrelated => return Err(BADRESP),
            }
        }
        Err(NODATA)
    }

    /// One question: `tries` rounds over every server. After the last attempt the error of
    /// that attempt is returned, as c-ares `ares_requeue_query` does.
    async fn query(&self, question: &Question, ttl: bool, local: (Ipv4Addr, Ipv6Addr)) -> Result<Reply, &'static str> {
        if self.servers.is_empty() {
            return Err(NO_SERVERS);
        }
        let mut tcp = false;
        let mut last = TIMEOUT;
        for attempt in 0..self.tries.saturating_mul(self.servers.len()) {
            let server = self.servers[attempt % self.servers.len()];
            let round = u32::try_from(attempt / self.servers.len()).unwrap_or(MAX_ROUNDS).min(MAX_ROUNDS);
            let wait = self.attempt_wait(round)?;
            let id = random_u16()?;
            let source = match server {
                SocketAddr::V4(_) => IpAddr::V4(local.0),
                SocketAddr::V6(_) => IpAddr::V6(local.1),
            };
            let exchange = Attempt { question, packet: question.encode(id), id, ttl, server, source };
            let mut result = if tcp { exchange.tcp(wait).await } else { exchange.udp(wait).await };
            if matches!(result, Ok(Reply::Truncated)) {
                tcp = true;
                result = exchange.tcp(wait).await;
            }
            match result {
                Ok(Reply::Truncated | Reply::Unrelated) => last = BADRESP,
                Ok(reply) => return Ok(reply),
                Err(code) if retryable(code) => last = code,
                Err(code) => return Err(code),
            }
        }
        Err(last)
    }
}
