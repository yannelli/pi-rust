//! DNS record queries, retry timing, and server selection for a single Node resolver.

mod health;
mod tcp;
#[cfg(test)]
mod tests;
mod timer;
mod transport;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

use super::wire::{Kind, Question, Reply};
use health::Server;
use timer::{Running, Timer};
use transport::Attempt;

const MIN_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const CANCELLED: &str = "ECANCELLED";
pub(crate) const CONNREFUSED: &str = "ECONNREFUSED";
pub(crate) const TIMEOUT: &str = "ETIMEOUT";
pub(crate) const BADRESP: &str = "EBADRESP";
pub(crate) const NO_SERVERS: &str = "UNKNOWN_ARES_ERROR";

pub(crate) struct Resolver {
    servers: Vec<Arc<Server>>,
    timeout: Duration,
    tries: usize,
    max_timeout: Option<Duration>,
    rotate: bool,
    lifetime: CancellationToken,
    timer: Arc<Timer>,
}

impl Drop for Resolver {
    fn drop(&mut self) {
        self.lifetime.cancel();
    }
}

fn random_u16() -> Result<u16, &'static str> {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes).map_err(|_| "ENOMEM")?;
    Ok(u16::from_ne_bytes(bytes))
}

impl Resolver {
    pub(crate) fn new(
        servers: Vec<SocketAddr>,
        timeout: Duration,
        tries: usize,
        max_timeout: Option<Duration>,
    ) -> Self {
        Self {
            servers: servers.into_iter().map(|address| Arc::new(Server::new(address))).collect(),
            timeout,
            tries: tries.max(1),
            max_timeout,
            rotate: false,
            lifetime: CancellationToken::new(),
            timer: Arc::new(Timer::new(timeout)),
        }
    }

    pub(super) fn with_rotation(mut self, rotate: bool) -> Self {
        self.rotate = rotate;
        self
    }

    pub(super) fn with_servers(&self, addresses: Vec<SocketAddr>) -> Self {
        let servers = addresses
            .into_iter()
            .map(|address| {
                self.servers
                    .iter()
                    .find(|server| server.address == address)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(Server::new(address)))
            })
            .collect();
        Self {
            servers,
            timeout: self.timeout,
            max_timeout: self.max_timeout,
            tries: self.tries,
            rotate: self.rotate,
            lifetime: CancellationToken::new(),
            timer: Arc::clone(&self.timer),
        }
    }

    pub(super) fn ordered_servers(&self) -> Vec<SocketAddr> {
        let mut ranked: Vec<_> = self.servers.iter().map(|server| (server.priority(), server.address)).collect();
        ranked.sort_by_key(|&(priority, _)| priority);
        ranked.into_iter().map(|(_, address)| address).collect()
    }

    fn preferred(&self) -> Result<&Arc<Server>, &'static str> {
        if let [server] = &self.servers[..] {
            return Ok(server);
        }
        let best = self.servers.iter().min_by_key(|server| server.priority()).ok_or(NO_SERVERS)?;
        if !self.rotate {
            return Ok(best);
        }
        let fewest_failures = best.priority().0;
        let eligible: Vec<_> = self.servers.iter().filter(|server| server.priority().0 == fewest_failures).collect();
        Ok(if eligible.is_empty() { best } else { eligible[usize::from(random_u16()?) % eligible.len()] })
    }

    fn attempt_wait(&self, initial: Duration, round: usize) -> Result<Duration, &'static str> {
        let base = initial.max(MIN_TIMEOUT).min(self.max_timeout.unwrap_or(MAX_TIMEOUT));
        if round == 0 {
            return Ok(base);
        }
        let mut wait = base.saturating_mul(1 << round.min(16));
        if let Some(max) = self.max_timeout {
            wait = wait.min(max);
        }
        let jitter = f64::from(random_u16()?) / f64::from(u16::MAX) * 0.5;
        Ok(wait.mul_f64(1.0 - jitter).max(base))
    }

    #[cfg(test)]
    async fn resolve(
        &self,
        name: &str,
        kind: Kind,
        ttl: bool,
        local: (Ipv4Addr, Ipv6Addr),
        cancel: CancellationToken,
    ) -> Result<Json, &'static str> {
        let question = self.prepare(name, kind)?;
        self.resolve_question(&question, ttl, local, cancel).await
    }

    pub(super) fn prepare(&self, name: &str, kind: Kind) -> Result<Question, &'static str> {
        let name_bytes = name.strip_suffix('.').unwrap_or(name).as_bytes();
        if name_bytes.last_chunk::<6>().is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".onion")) {
            return Err("ENOTFOUND");
        }
        if self.servers.is_empty() {
            return Err(NO_SERVERS);
        }
        Question::new(name, kind)
    }

    pub(super) async fn resolve_question(
        &self,
        question: &Question,
        ttl: bool,
        local: (Ipv4Addr, Ipv6Addr),
        cancel: CancellationToken,
    ) -> Result<Json, &'static str> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CANCELLED),
            result = self.query(question, ttl, local, &cancel) => result,
        }
    }

    async fn query(
        &self,
        question: &Question,
        ttl: bool,
        local: (Ipv4Addr, Ipv6Addr),
        cancel: &CancellationToken,
    ) -> Result<Json, &'static str> {
        let id = random_u16()?;
        let packet = question.encode(id);
        let timer = self.timer.enter();
        let mut tcp = false;
        let mut last = NO_SERVERS;
        for index in 0..self.tries.saturating_mul(self.servers.len()) {
            let server = self.preferred()?;
            let wait = self.attempt_wait(server.timeout(self.timeout), index / self.servers.len())?;
            if index == 0 {
                self.probe_failed(server, question, local, cancel);
            }
            let exchange = Attempt::new(question, &packet, id, ttl, server.address, local);
            match exchange.run(server, wait, &mut tcp, &timer).await {
                Ok(Reply::Answer { value, .. }) => return Ok(value),
                Ok(Reply::Error { code, .. }) => return Err(code),
                Ok(_) => return Err(BADRESP),
                Err(code @ (TIMEOUT | CONNREFUSED | BADRESP)) => last = code,
                Err(code) => return Err(code),
            }
        }
        Err(last)
    }

    fn probe_failed(
        &self,
        selected: &Server,
        question: &Question,
        local: (Ipv4Addr, Ipv6Addr),
        cancel: &CancellationToken,
    ) {
        if self.servers.len() < 2 {
            return;
        }
        let candidate = self
            .servers
            .iter()
            .filter(|server| server.address != selected.address)
            .filter_map(|server| server.probe_priority().map(|priority| (priority, server)))
            .min_by_key(|(priority, _)| *priority);
        let Some((_, server)) = candidate else { return };
        let Ok(chance) = random_u16() else { return };
        if chance % 10 != 0 || !server.begin_probe() {
            return;
        }
        let server = Arc::clone(server);
        let question = question.clone();
        let (cancel, lifetime) = (cancel.clone(), self.lifetime.clone());
        let timer = Arc::clone(&self.timer);
        let wait = server.timeout(self.timeout).max(MIN_TIMEOUT).min(self.max_timeout.unwrap_or(MAX_TIMEOUT));
        tokio::spawn(async move {
            let probe = async {
                let id = random_u16()?;
                let packet = question.encode(id);
                Attempt::new(&question, &packet, id, false, server.address, local)
                    .run(&server, wait, &mut false, &timer.enter())
                    .await
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => {},
                () = lifetime.cancelled() => {},
                _ = probe => {},
            }
            server.end_probe();
        });
    }
}

impl<'a> Attempt<'a> {
    fn new(
        question: &'a Question,
        packet: &'a [u8],
        id: u16,
        ttl: bool,
        server: SocketAddr,
        local: (Ipv4Addr, Ipv6Addr),
    ) -> Self {
        let source = if server.is_ipv4() { IpAddr::V4(local.0) } else { IpAddr::V6(local.1) };
        Self { question, packet, id, ttl, server, source }
    }

    async fn exchange(
        &self,
        server: &Server,
        wait: Duration,
        tcp: bool,
        timer: &Running<'_>,
    ) -> Result<Reply, &'static str> {
        let deadline = tokio::time::Instant::now() + wait;
        let io = async { if tcp { self.tcp(&server.tcp).await } else { self.udp(timer).await } };
        let result = tokio::select! {
            biased;
            result = io => result,
            () = timer.expired(deadline) => Err(TIMEOUT),
        };
        timer.activity();
        result
    }

    async fn run(
        &self,
        server: &Server,
        wait: Duration,
        tcp: &mut bool,
        timer: &Running<'_>,
    ) -> Result<Reply, &'static str> {
        let mut started = Instant::now();
        let mut result = self.exchange(server, wait, *tcp, timer).await;
        if matches!(result, Ok(Reply::Truncated)) {
            *tcp = true;
            started = Instant::now();
            result = self.exchange(server, wait, true, timer).await;
        }
        match &result {
            Ok(Reply::Answer { measure, .. } | Reply::Error { measure, .. }) => {
                server.responded(measure.then(|| started.elapsed()))
            }
            Err(TIMEOUT | CONNREFUSED | BADRESP) => server.failed(),
            _ => {}
        }
        result
    }
}
