//! Port of upstream `test/requests.test.ts`, reload cases: #3 (listener subscriptions across reloads), #5 (roots
//! of a child that connected behind) and #7 (what each child prepared). Upstream compares a listener's `select`
//! by its source text; the key given to `on_select` stands for it here.

mod chord2_support;

use std::sync::{Arc, Mutex};

use chord2_support::cluster::{Modules, remote_options};
use chord2_support::net::host_options;
use chord2_support::*;
use pi_chord::host2::declare::{ResourceDeclaration, Subscribable};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, Facet, GetOptions, HostOptions, Implementation, Listenable,
    RemoteDef, RootOptions, VariantDef, create_host, define_facet, host_named, memory_link_pair, method, remote,
};
use pi_chord::{Json, Unsubscribe, json};

type Listener = Arc<dyn Fn(&Json) + Send + Sync>;

/// A source that takes one subscriber at a time (like a Durable watch), unless `many`; `starts` counts every
/// subscription.
#[derive(Clone, Default)]
struct Single(Arc<Mutex<(usize, usize, Option<Listener>)>>, bool);

impl Single {
    fn emit(&self, event: &str) {
        let listener = self.0.lock().unwrap().2.clone();
        if let Some(listener) = listener {
            listener(&json!(event));
        }
    }

    fn subscribers(&self) -> usize {
        self.0.lock().unwrap().0
    }

    fn starts(&self) -> usize {
        self.0.lock().unwrap().1
    }
}

impl Subscribable<Json> for Single {
    fn subscribe(&self, listener: Box<dyn Fn(&Json) + Send + Sync>) -> Unsubscribe {
        let mut state = self.0.lock().unwrap();
        assert!(self.1 || state.0 == 0, "Watch is already started");
        state.0 += 1;
        state.1 += 1;
        state.2 = Some(Arc::from(listener));
        let inner = self.0.clone();
        Unsubscribe::new(move || {
            let mut state = inner.lock().unwrap();
            state.0 -= 1;
            state.2 = None;
        })
    }
}

#[tokio::test]
async fn r3_a_listener_keeps_its_subscription_when_resource_and_select_are_unchanged() {
    let _serial = serial().await;
    let watcher = local_type("t.watcher", object(&[]), vec![("seen", array(string()).into())]);
    let single = Single::default();
    let facet = |tag: &'static str| {
        let (watcher, single) = (watcher.clone(), single.clone());
        define_facet("watcher", move |api| {
            let single = single.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let seen = decl.state("seen", array(string()), json!([]))?;
                let watch = decl.resource("watch", ResourceDeclaration::value(1, single.clone()))?;
                // A new adapter per call, and a new closure per factory run: the key says it is the same select.
                let select = |watch: &Single| Ok(Listenable::Events(Arc::new(watch.clone())));
                let target = seen.clone();
                decl.on_select("events", &watch, "(watch) => adapter(watch)", select, move |event, ctx| {
                    let changed = target.change(&ctx, |draft| draft.push(json!(format!("{tag}:{}", text(&event)))));
                    std::future::ready(changed)
                })?;
                Ok(Implementation::new().state("seen", seen))
            };
            api.serve(&watcher, factory, Default::default())
        })
    };
    let host = quiet();
    host.install(&facet("v1"), Default::default()).await.unwrap();
    let reference = host.hold(&watcher, json!({}), GetOptions::default()).unwrap();
    reference.ready().await.unwrap();
    single.emit("a");
    for tag in ["v2", "v3"] {
        host.reload(vec![facet(tag).into()], Default::default()).await.unwrap();
    }
    single.emit("b");
    sleep_ms(5).await;
    assert_eq!((single.subscribers(), single.starts()), (1, 1));
    assert_eq!(reference.state("seen").unwrap().value().unwrap(), json!(["v1:a", "v3:b"]));
}

#[tokio::test]
async fn r3_a_changed_select_resubscribes() {
    let _serial = serial().await;
    let watcher = local_type("t.watcher2", object(&[]), vec![]);
    let source = Single(Arc::default(), true);
    let facet = |key: &'static str| {
        let (watcher, source) = (watcher.clone(), source.clone());
        define_facet("watcher2", move |api| {
            let source = source.clone();
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let resource = decl.resource("s", ResourceDeclaration::value(1, source.clone()))?;
                let select = |s: &Single| Ok(Listenable::Events(Arc::new(s.clone())));
                decl.on_select("s", &resource, key, select, |_, _| std::future::ready(Ok(())))?;
                Ok(Implementation::new())
            };
            api.serve(&watcher, factory, Default::default())
        })
    };
    let host = quiet();
    host.install(&facet("(s) => s"), Default::default()).await.unwrap();
    host.hold(&watcher, json!({}), GetOptions::default()).unwrap().ready().await.unwrap();
    host.reload(vec![facet("(s) => s").into()], Default::default()).await.unwrap();
    assert_eq!(source.starts(), 1);
    host.reload(vec![facet("(value) => value").into()], Default::default()).await.unwrap();
    assert_eq!(source.starts(), 2);
}

fn on_server(name: &str, host: &str, method_name: &str, spec: pi_chord::host2::MethodSpec) -> Contract {
    let main = VariantDef::new().method(method_name, spec);
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named(host).unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote(name, &def).unwrap()
}

#[tokio::test]
async fn r5_roots_of_a_child_connected_behind_are_admitted_in_the_server_code_version() {
    let _serial = serial().await;
    let echo = on_server("t.echo", "server", "codeVersion", method(None, number()));
    let served = echo.clone();
    let echo_facet = define_facet("echo", move |api| {
        let factory = |_: &Json, _: &Json, _: &mut Declare| {
            let code_version = |_, ctx: pi_chord::host2::Ctx| std::future::ready(Ok(json!(ctx.code_version())));
            Ok(Implementation::variant("main").method("codeVersion", code_version))
        };
        api.serve(&served, factory, Default::default())
    });
    let server = create_host(host_options("server", &Log::default()));
    server.install(&echo_facet, Default::default()).await.unwrap();
    server.reload(vec![echo_facet.into()], Default::default()).await.unwrap();
    assert_eq!(server.code_version(), 2);
    assert_eq!(server.previous_code_version(), None);
    let worker = create_host(host_options("session:S1", &Log::default()));
    let go = Latch::default();
    // A worker root of code version 1 (an eager object's listener, say) calls the server after connecting.
    let call = {
        let (go, echo) = (go.clone(), echo.clone());
        tokio::spawn(worker.root(
            move |ctx| async move {
                go.wait().await;
                let echo = ctx.get(&echo, json!({}), GetOptions::default()).await?;
                echo.call_json("codeVersion", Json::Null, Some(&ctx)).await
            },
            RootOptions::default(),
        ))
    };
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::host("session:S1", json!({"worker": "S1"}))).unwrap();
    worker.connect(up, ConnectOptions::new(json!({"kind": "server"})));
    go.open(Json::Null);
    assert_eq!(call.await.unwrap().unwrap(), json!(2));
    until(|| worker.code_version() == 2).await;
    // Unknown code versions from in-step peers stay superseded.
    let options = RootOptions { code_version: Some(1), ..RootOptions::default() };
    let error = server.root(|_| async { Ok(0) }, options).await.unwrap_err();
    assert!(is(&error, "superseded"), "{error}");
}

fn greeter_facet(greeter: &Contract, text: &'static str) -> Facet {
    let greeter = greeter.clone();
    define_facet("greeter", move |api| {
        let factory = move |_: &Json, _: &Json, _: &mut Declare| {
            Ok(Implementation::variant("main").method("greet", move |name, _| {
                std::future::ready(Ok(json!(format!("{text} {}", chord2_support::text(&name)))))
            }))
        };
        api.serve(&greeter, factory, Default::default())
    })
}

#[tokio::test]
async fn r7_a_cluster_reload_reports_what_each_child_prepared() {
    let _serial = serial().await;
    let greeter = on_server("t.greeter", "session:S1", "greet", method(string(), string()));
    let modules = Modules::with("greeter", greeter_facet(&greeter, "v1"));
    let server = create_host(host_options("server", &Log::default()));
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::host("session:S1", json!({"worker": "S1"}))).unwrap();
    let upstream: Option<(Arc<dyn pi_chord::host2::Link>, ConnectOptions)> =
        Some((up, ConnectOptions::new(json!({"kind": "server"}))));
    let options = HostOptions { upstream, loader: modules.loader(), ..host_options("session:S1", &Log::default()) };
    let worker = create_host(options);
    worker.install(&greeter_facet(&greeter, "v1"), Default::default()).await.unwrap();
    tick().await;
    let reference = server.hold(&greeter, json!({}), GetOptions::default()).unwrap();
    assert_eq!(reference.call_json("greet", json!("a"), None).await.unwrap(), json!("v1 a"));
    modules.set("greeter", greeter_facet(&greeter, "v2"));
    let report = server.reload(vec![], remote_options(&["greeter"])).await.unwrap();
    assert!(report.changed.is_empty());
    let child = &report.children["session:S1"];
    assert_eq!(child.changed, ["t.greeter"]);
    assert_eq!(child.prepared.len(), 1);
    assert_eq!(child.code_version, report.code_version);
}
