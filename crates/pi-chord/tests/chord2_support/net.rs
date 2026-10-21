//! Helpers for the link tests (upstream `test/link.test.ts`): the counter contracts, hosts joined by memory
//! links, and a cluster whose server routes `session:` hosts to workers it spawns on demand.

use std::collections::HashMap;
use std::future::ready;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::link::MemoryLink;
use pi_chord::host2::wire::PeerKind;
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, Host, HostOptions, Implementation, Link, LinkOptions, Readable,
    Ref, RemoteDef, Restamp, Route, ServeOptions, VariantDef, create_host, host_family, host_named, memory_link_pair,
    method, remote,
};
use pi_chord::{ChordError, ChordResult, Json, json};

use super::{Log, number, object, req, string, unknown};

pub type Memory = Arc<MemoryLink>;

pub fn counter_variants() -> Vec<(String, VariantDef)> {
    let main = VariantDef::new()
        .state("count", object(&[req("n", number())]))
        .method("add", method(number(), number()))
        .method("who", method(None, unknown()));
    vec![("main".into(), main)]
}

/// A counter on the server.
pub fn counter() -> Contract {
    let input = object(&[req("name", string())]);
    let def = RemoteDef { version: 1, input, host: host_named("server").unwrap(), variants: counter_variants() };
    remote("t.counter", &def).unwrap()
}

/// A counter on a session host (`session:` + its `session` field).
pub fn session_counter() -> Contract {
    let input = object(&[req("session", string()), req("name", string())]);
    let host = host_family("session:", "session").unwrap();
    remote("t.session-counter", &RemoteDef { version: 1, input, host, variants: counter_variants() }).unwrap()
}

/// The counter's implementation for an identity; `log` gets `open <name>` and `close <name>` of a probe resource.
pub fn counter_impl(
    input: &Json,
    principal: &Json,
    decl: &mut Declare,
    log: Option<&Log>,
) -> ChordResult<Implementation> {
    if let Some(log) = log {
        let name = input["name"].as_str().unwrap_or_default().to_string();
        let (opened, closed, label) = (log.clone(), log.clone(), name.clone());
        let open = move |_: OpenOptions| opened.push_ready(format!("open {label}"));
        let close = move |()| closed.push_ready(format!("close {name}"));
        decl.resource("probe", ResourceDeclaration::new(1, open).with_close(close))?;
    }
    let count = decl.state("count", object(&[req("n", number())]), json!({"n": 0}))?;
    let principal = principal.clone();
    Ok(Implementation::variant("main")
        .state("count", count.clone())
        .method("add", move |delta, ctx| {
            let count = count.clone();
            async move {
                let delta = delta.as_f64().unwrap_or_default();
                count.change(&ctx, |draft| {
                    let n = draft["n"].as_f64().unwrap_or_default();
                    draft.set("n", n + delta)
                })?;
                Ok(count.value()?["n"].clone())
            }
        })
        .method("who", move |_, _| ready(Ok(principal.clone()))))
}

/// Serves the counter of the host's kind (`counter` on the server, `session_counter` elsewhere) and records the
/// identity of every object it creates.
pub fn serve_counter(host: &Host, principals: &Log<Json>) {
    let principals = principals.clone();
    let contract = if host.name() == "server" { counter() } else { session_counter() };
    let factory = move |input: &Json, principal: &Json, decl: &mut Declare| {
        principals.push(principal.clone());
        counter_impl(input, principal, decl, None)
    };
    host.serve(&contract, factory, ServeOptions::default()).unwrap();
}

pub fn host_options(name: &str, errors: &Log<ChordError>) -> HostOptions {
    let errors = errors.clone();
    HostOptions {
        name: Some(name.into()),
        principal: json!({"kind": "host"}),
        on_error: Some(Arc::new(move |error| errors.push(error))),
        ..HostOptions::default()
    }
}

/// A host whose errors are dropped.
pub fn named(name: &str) -> Host {
    create_host(host_options(name, &Log::default()))
}

/// A browser (client host) whose parent is reached over `up`.
pub fn browser_on(up: &Memory, link: LinkOptions) -> Host {
    let upstream = ConnectOptions { principal: json!({"kind": "server"}), link };
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up.clone(), upstream));
    create_host(HostOptions { kind: PeerKind::Client, upstream, ..host_options("browser", &Log::default()) })
}

/// A child host named `name` whose parent is reached over `up`.
pub fn child_on(name: &str, up: &Memory, link: LinkOptions, errors: &Log<ChordError>) -> Host {
    let upstream = ConnectOptions { principal: json!({"kind": "server"}), link };
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up.clone(), upstream));
    create_host(HostOptions { upstream, ..host_options(name, errors) })
}

pub struct Pair {
    pub server: Host,
    pub worker: Host,
    pub down: Memory,
    pub up: Memory,
    pub errors: Log<ChordError>,
}

/// A server and a worker connected by a memory link; the server accepts the worker as `{worker: name}`.
pub fn server_with_worker(worker_name: &str) -> Pair {
    let errors = Log::default();
    let server = create_host(host_options("server", &errors));
    let (down, up) = memory_link_pair();
    server.accept(down.clone(), AcceptOptions::host(worker_name, json!({"worker": worker_name}))).unwrap();
    let worker = child_on(worker_name, &up, LinkOptions::default(), &errors);
    Pair { server, worker, down, up, errors }
}

/// Waits until `host` has `count` connections (its `connect` or `accept` is handled in its link task).
pub async fn connected(host: &Host, count: usize) {
    for _ in 0..200 {
        if host.connections().await.len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{} has no connection", host.name());
}

/// Calls `member` without a context (a fresh root); `null` for a void result.
pub async fn call(reference: &Ref, member: &str, args: Json) -> ChordResult<Json> {
    Ok(reference.call(member, Some(args), None).await?.unwrap_or_default())
}

type Allow = Arc<dyn Fn(&Json, &str) -> bool + Send + Sync>;

/// A server routing `session:*` to workers it spawns on demand (memory links); browsers connect as clients.
#[derive(Clone)]
pub struct Cluster {
    pub server: Host,
    pub workers: Arc<Mutex<HashMap<String, Host>>>,
    pub log: Log,
    allow: Arc<Mutex<Allow>>,
    link: LinkOptions,
}

/// A browser of a cluster: its host and the current link to the server.
pub struct Browser {
    pub host: Host,
    pub up: Mutex<Memory>,
    server: Host,
    principal: Json,
}

impl Browser {
    pub fn up(&self) -> Memory {
        self.up.lock().unwrap().clone()
    }

    pub fn reconnect(&self) {
        let (down, up) = memory_link_pair();
        self.server.accept(down, AcceptOptions::client(self.principal.clone())).unwrap();
        *self.up.lock().unwrap() = up.clone();
        self.host.connect(up, ConnectOptions::new(json!({"kind": "server"})));
    }
}

impl Cluster {
    pub fn new(link: LinkOptions) -> Self {
        let server = named("server");
        let state = Self {
            server: server.clone(),
            workers: Arc::default(),
            log: Log::default(),
            allow: Arc::new(Mutex::new(Arc::new(|_: &Json, _: &str| true))),
            link,
        };
        let (allow, workers, log, link) =
            (state.allow.clone(), state.workers.clone(), state.log.clone(), state.link.clone());
        let authorize: pi_chord::host2::Authorize = Arc::new(move |principal: &Json, host: &str| {
            let allowed = (allow.lock().unwrap().clone())(principal, host);
            Box::pin(ready(allowed))
        });
        let resolve: pi_chord::host2::Resolve = Arc::new(move |name: &str| {
            let (down, up) = memory_link_pair();
            let worker = child_on(name, &up, link.clone(), &Log::default());
            let log = log.clone();
            let factory = move |input: &Json, principal: &Json, decl: &mut Declare| {
                counter_impl(input, principal, decl, Some(&log))
            };
            worker.serve(&session_counter(), factory, ServeOptions::default()).unwrap();
            workers.lock().unwrap().insert(name.to_string(), worker);
            let down: Arc<dyn Link> = down;
            Box::pin(ready(Ok(down)))
        });
        let principal = Arc::new(|name: &str| json!({"host": name}));
        server.route("session:", Route { authorize, resolve, principal });
        state
    }

    pub fn set_allow(&self, allow: impl Fn(&Json, &str) -> bool + Send + Sync + 'static) {
        *self.allow.lock().unwrap() = Arc::new(allow);
    }

    pub fn worker_count(&self) -> usize {
        self.workers.lock().unwrap().len()
    }

    pub fn browser(&self, principal: Json, restamp: Option<Restamp>) -> Browser {
        let (down, up) = memory_link_pair();
        let options = AcceptOptions { restamp, link: self.link.clone(), ..AcceptOptions::client(principal.clone()) };
        self.server.accept(down, options).unwrap();
        let host = browser_on(&up, self.link.clone());
        Browser { host, up: Mutex::new(up), server: self.server.clone(), principal }
    }
}

/// Link options with fast pings (the dead-link test).
pub fn pings(interval_ms: u64, timeout_ms: u64) -> LinkOptions {
    LinkOptions {
        ping_interval: Some(Duration::from_millis(interval_ms)),
        ping_timeout: Some(Duration::from_millis(timeout_ms)),
        ..LinkOptions::default()
    }
}
