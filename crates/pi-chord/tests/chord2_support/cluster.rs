//! Helpers for the cluster reload tests (upstream `test/cluster.test.ts`): the greeter types and facet, module
//! loaders that can wait for a latch, a link that drops chosen frames, and a server with one session worker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::Frame;
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, Facet, FacetLoader, Host, HostOptions, Implementation, Initial,
    Link, LinkOptions, RemoteDef, StateOptions, VariantDef, create_host, define_facet, host_family, host_named,
    memory_link_pair, method, remote,
};
use pi_chord::{ChordError, ChordResult, Json, json};

use super::net::{Memory, host_options};
use super::{Latch, Log, number, object, req, string, text};

fn session_def(version: i64, main: VariantDef) -> RemoteDef {
    let input = object(&[req("session", string())]);
    RemoteDef {
        version,
        input,
        host: host_family("session:", "session").unwrap(),
        variants: vec![("main".into(), main)],
    }
}

fn greeter_main() -> VariantDef {
    VariantDef::new().state("count", object(&[req("n", number())])).method("greet", method(string(), string()))
}

/// The greeter on session hosts.
pub fn greeter() -> Contract {
    remote("t.greeter", &session_def(1, greeter_main())).unwrap()
}

pub fn greeter_v2() -> Contract {
    let main = VariantDef::new().method("hello", method(string(), string()));
    remote("t.greeter", &session_def(2, main)).unwrap()
}

/// The server's own greeter: the greeter facet serves it on the server, [`greeter`] on session hosts.
pub fn server_greeter() -> Contract {
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named("server").unwrap(),
        variants: vec![("main".into(), greeter_main())],
    };
    remote("t.server-greeter", &def).unwrap()
}

pub fn greeter_facet(greeting: &str) -> Facet {
    greeter_facet_with(greeting, false, 1)
}

pub fn greeter_facet_with(greeting: &str, fail: bool, state_version: i64) -> Facet {
    let greeting = greeting.to_string();
    define_facet("greeter", move |api| {
        let greeting = greeting.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            if fail {
                return Err(ChordError::error("worker factory fails"));
            }
            let options = StateOptions { version: Some(state_version), migrate: None };
            let first = Initial::Value(json!({"n": 0}));
            let count = decl.state_with("count", object(&[req("n", number())]), first, options)?;
            let (greeting, counted) = (greeting.clone(), count.clone());
            Ok(Implementation::variant("main").state("count", count).method("greet", move |name, ctx| {
                let (count, greeting) = (counted.clone(), greeting.clone());
                async move {
                    count.change(&ctx, |draft| {
                        let n = draft["n"].as_f64().unwrap_or_default();
                        draft.set("n", n + 1.0)
                    })?;
                    Ok(json!(format!("{greeting} {}", text(&name))))
                }
            }))
        };
        let contract = if api.name() == "server" { server_greeter() } else { greeter() };
        api.serve(&contract, factory, Default::default())
    })
}

/// A facet that serves nothing.
pub fn empty_facet(name: &str) -> Facet {
    define_facet(name, |_| Ok(()))
}

/// Facets by module name; a load waits for `gate` while `blocked` (upstream's test loaders).
#[derive(Clone, Default)]
pub struct Modules {
    facets: Arc<Mutex<HashMap<String, Facet>>>,
    /// Loads build empty facets named by their modules instead.
    stubs: bool,
    blocked: Arc<AtomicBool>,
    gate: Latch,
}

impl Modules {
    pub fn with(name: &str, facet: Facet) -> Self {
        let modules = Self::default();
        modules.set(name, facet);
        modules
    }

    /// A loader of empty facets (a server's own plugin modules).
    pub fn stubs() -> Self {
        Self { stubs: true, ..Self::default() }
    }

    pub fn set(&self, name: &str, facet: Facet) {
        self.facets.lock().unwrap().insert(name.to_string(), facet);
    }

    pub fn get(&self, name: &str) -> Facet {
        self.facets.lock().unwrap()[name].clone()
    }

    pub fn block(&self, blocked: bool) {
        self.blocked.store(blocked, Ordering::SeqCst);
    }

    pub fn open(&self) {
        self.gate.open(Json::Null);
    }

    pub fn loader(&self) -> Option<Arc<dyn FacetLoader<Facet>>> {
        Some(Arc::new(self.clone()))
    }
}

#[async_trait]
impl FacetLoader<Facet> for Modules {
    async fn load(&self, modules: &[String]) -> ChordResult<Vec<Facet>> {
        if self.blocked.load(Ordering::SeqCst) {
            self.gate.wait().await;
        }
        if self.stubs {
            return Ok(modules.iter().map(|name| empty_facet(name)).collect());
        }
        let facets = self.facets.lock().unwrap();
        Ok(modules.iter().map(|name| facets[name.as_str()].clone()).collect())
    }
}

/// A loader that must not be asked for anything.
pub struct NoModules;

#[async_trait]
impl FacetLoader<Facet> for NoModules {
    async fn load(&self, modules: &[String]) -> ChordResult<Vec<Facet>> {
        panic!("removed modules must not be loaded: {modules:?}");
    }
}

pub type Drop = Arc<dyn Fn(&Frame) -> bool + Send + Sync>;

/// `link`, except that the frames `drop` selects are never sent (a frame lost when its link dies).
pub struct Dropping {
    pub link: Arc<dyn Link>,
    pub drop: Drop,
}

impl Dropping {
    pub fn wrap(link: Arc<dyn Link>, drop: impl Fn(&Frame) -> bool + Send + Sync + 'static) -> Arc<dyn Link> {
        Arc::new(Self { link, drop: Arc::new(drop) })
    }
}

impl Link for Dropping {
    fn closed(&self) -> bool {
        self.link.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        if (self.drop)(frame) { None } else { self.link.send(frame) }
    }

    fn receive(&self, handler: FrameHandler) {
        self.link.receive(handler);
    }

    fn on_close(&self, handler: CloseHandler) {
        self.link.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        self.link.close(reason);
    }
}

pub fn server() -> Host {
    create_host(host_options("server", &Log::default()))
}

pub fn server_loading(modules: &Modules) -> Host {
    create_host(HostOptions { loader: modules.loader(), ..host_options("server", &Log::default()) })
}

/// A session host named `session:{name}` the server accepted, connected over a new memory link (both ends given).
pub fn worker(server: &Host, name: &str, modules: Option<&Modules>) -> (Host, Memory, Memory) {
    let (down, up) = memory_link_pair();
    let host_name = format!("session:{name}");
    server.accept(down.clone(), AcceptOptions::host(&host_name, json!({"worker": name}))).unwrap();
    let upstream = ConnectOptions { principal: json!({"kind": "server"}), link: LinkOptions::default() };
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up.clone(), upstream));
    let loader = modules.and_then(Modules::loader);
    (create_host(HostOptions { upstream, loader, ..host_options(&host_name, &Log::default()) }), up, down)
}

/// A session host that connects later (`connect`), with its own loader.
pub fn unconnected(name: &str, loader: Option<Arc<dyn FacetLoader<Facet>>>) -> Host {
    create_host(HostOptions { loader, ..host_options(&format!("session:{name}"), &Log::default()) })
}

/// Connects `host` to `server` as a child over a new memory link.
pub fn join(server: &Host, host: &Host, name: &str) {
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::host(&format!("session:{name}"), json!({"worker": name}))).unwrap();
    host.connect(up, ConnectOptions::new(json!({"kind": "server"})));
}

pub struct Setup {
    pub server: Host,
    pub worker: Host,
    pub modules: Modules,
    pub up: Memory,
    pub down: Memory,
}

/// Server and worker `session:S1` over a memory link; the worker loads facets by name from `modules`.
pub async fn setup() -> Setup {
    let modules = Modules::with("greeter", greeter_facet("v1"));
    let server = server();
    let (worker, up, down) = worker(&server, "S1", Some(&modules));
    worker.install(&modules.get("greeter"), Default::default()).await.unwrap();
    Setup { server, worker, modules, up, down }
}

/// `{remote: {"session:": modules}}`.
pub fn remote_options(modules: &[&str]) -> pi_chord::host2::ReloadOptions {
    let modules = modules.iter().map(|module| module.to_string()).collect();
    pi_chord::host2::ReloadOptions { remote: Some([("session:".to_string(), modules)].into()), ..Default::default() }
}
