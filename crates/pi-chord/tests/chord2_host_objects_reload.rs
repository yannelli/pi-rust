//! Port of upstream `test/host-objects-reload.test.ts`: the host's own references to `lifetime: "host"` objects
//! across reloads. Upstream reads `HostImpl.hostObjects`; [`Host::host_object`] is the public view of it here.

mod chord2_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chord2_support::net::host_options;
use chord2_support::reload::*;
use chord2_support::*;
use pi_chord::host2::declare::ResourceDeclaration;
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, Facet, HostOptions, Implementation, InProcessLoader, LoadOptions,
    Readable, RefStatus, Registry, RegistryView, ReloadOptions, ServeOptions, StateOptions, create_host, define_facet,
    define_registry, memory_link_pair, method,
};
use pi_chord::{ChordError, Json, json};

type Reader = Arc<dyn Fn() -> Json + Send + Sync>;

fn flags() -> Json {
    json!({"type": "object", "patternProperties": {"^.*$": {"type": "boolean"}}})
}

fn switch(version: i64) -> Contract {
    let toggle = object(&[req("conversation", number()), req("enabled", boolean())]);
    let members = match version {
        1 => vec![("enabled", boolean().into()), ("set", method(boolean(), None).into())],
        2 => vec![("enabled", flags().into()), ("set", method(toggle, None).into())],
        _ => vec![("enabled", flags().into()), ("update", method(toggle, None).into())],
    };
    local_version("t.eager-switch", version, members)
}

fn read_registry() -> Registry<Reader> {
    define_registry("t.eager-switch-read", None)
}

/// A switch facet: `version` 1 holds a boolean, 2 migrates it into flags (`valid`: into a valid value), 3 only
/// renames the method.
fn switch_facet(log: &Log, version: i64, valid: bool) -> Facet {
    let log = log.clone();
    define_facet("switch", move |api| {
        let log = log.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let (opens, closes) = (log.clone(), log.clone());
            let watch = ResourceDeclaration::new(1, move |_| opens.push_ready("open"))
                .with_close(move |_: ()| closes.push_ready("close"));
            decl.resource("watch", watch)?;
            let enabled = match version {
                1 => decl.state("enabled", boolean(), json!(false))?,
                _ => {
                    let migrate = move |old: Json, from: i64| {
                        assert_eq!(from, 1);
                        Ok(if valid { json!({"1": old}) } else { json!("invalid migration") })
                    };
                    let migrate = (version == 2).then(|| Arc::new(migrate) as pi_chord::host2::object::Migrate);
                    let options = StateOptions { version: Some(2), migrate };
                    decl.state_with("enabled", flags(), pi_chord::host2::Initial::Value(json!({})), options)?
                }
            };
            let current = enabled.clone();
            let reader: Reader = Arc::new(move || current.value().unwrap_or_default());
            decl.register(&read_registry(), [("current".to_string(), reader)])?;
            let target = enabled.clone();
            let set = move |args: Json, ctx| {
                let enabled = target.clone();
                async move {
                    match version {
                        1 => enabled.replace(&ctx, args)?,
                        _ => enabled.change(&ctx, |draft| {
                            let key = args["conversation"].as_f64().unwrap_or_default().to_string();
                            draft.set(key.as_str(), args["enabled"].clone())
                        })?,
                    }
                    Ok(Json::Null)
                }
            };
            let name = if version == 3 { "update" } else { "set" };
            Ok(Implementation::new().state("enabled", enabled).method(name, set))
        };
        api.serve(&switch(version), factory, ServeOptions::host())
    })
}

fn current(host: &pi_chord::host2::Host) -> Json {
    host.registry(&read_registry()).get("current", None).unwrap().expect("registered")()
}

#[tokio::test]
async fn the_host_reference_follows_contract_bumps_without_reopening_and_failures_keep_the_old_one() {
    let _serial = serial().await;
    let errors: Log<ChordError> = Log::default();
    let host = create_host(HostOptions { dev: true, ..quiet_options(&errors) });
    let log: Log = Log::default();
    host.load(&[switch_facet(&log, 1, true)], LoadOptions::default()).await.unwrap();
    let initial = host.host_object("t.eager-switch").unwrap();
    let object = initial.object().unwrap();
    let original = hold(&host, &switch(1), json!({}));
    original.call("set", Some(json!(true)), None).await.unwrap();
    original.release();
    let code_version = host.code_version();
    let error = reload(&host, switch_facet(&log, 2, false)).await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    assert_eq!(host.code_version(), code_version);
    assert!(host.host_object("t.eager-switch").unwrap().ptr_eq(&initial));
    assert_eq!(initial.status(), RefStatus::Live);
    assert_eq!(current(&host), json!(true));
    let report = reload(&host, switch_facet(&log, 2, true)).await.unwrap();
    assert_eq!(report.gated.len(), 1);
    let updated = host.host_object("t.eager-switch").unwrap();
    assert_eq!(updated.contract().fingerprint(), switch(2).fingerprint());
    assert!(Arc::ptr_eq(&updated.object().unwrap(), &object));
    assert_eq!(initial.status(), RefStatus::Released);
    assert_eq!(object.ref_count(), 1, "the replacement is attached before the only old hold is released");
    assert_eq!(current(&host), json!({"1": true}));
    let modern = hold(&host, &switch(2), json!({}));
    modern.call("set", Some(json!({"conversation": 2, "enabled": true})), None).await.unwrap();
    modern.release();
    assert!(reload(&host, switch_facet(&log, 3, true)).await.unwrap().gated.is_empty());
    let latest = host.host_object("t.eager-switch").unwrap();
    assert_eq!(latest.contract().fingerprint(), switch(3).fingerprint());
    assert!(Arc::ptr_eq(&latest.object().unwrap(), &object));
    assert_eq!(updated.status(), RefStatus::Released);
    assert_eq!(current(&host), json!({"1": true, "2": true}));
    host.ready().await.unwrap();
    assert_eq!(log.all(), ["open"]);
    assert!(errors.is_empty(), "{:?}", errors.all());
    host.close().await.unwrap();
    assert_eq!(log.all(), ["open", "close"]);
}

fn read(version: i64) -> Contract {
    let name = if version == 1 { "read" } else { "fetch" };
    local_version("t.eager-read", version, vec![(name, method(None, string()).into())])
}

fn local_version(name: &str, version: i64, members: Vec<(&str, pi_chord::host2::Member)>) -> Contract {
    let members = members.into_iter().map(|(name, member)| (name.to_string(), member)).collect();
    pi_chord::host2::local(name, &pi_chord::host2::LocalDef { version, input: object(&[]), members }).unwrap()
}

fn reader(version: i64, adapt: bool) -> Facet {
    define_facet("read", move |api| {
        let value = if version == 1 { "one" } else { "two" };
        let name = if version == 1 { "read" } else { "fetch" };
        let factory = move |_: &Json, _: &Json, _: &mut Declare| {
            Ok(Implementation::new().method(name, move |_, _| std::future::ready(Ok(json!(value)))))
        };
        let mut options = ServeOptions::host();
        if adapt {
            let adapter: pi_chord::host2::object::Adapter = Arc::new(|current: &Implementation| {
                let fetch = current.get("fetch").cloned().expect("fetch");
                Implementation::new().member("read", fetch)
            });
            options.accepts = Some([(1, adapter)].into());
        }
        api.serve(&read(version), factory, options)
    })
}

#[tokio::test]
async fn old_version_consumers_refuse_an_upgrade_without_accepts_and_keep_their_adapter_with_it() {
    let _serial = serial().await;
    let errors: Log<ChordError> = Log::default();
    let host = quiet_with(&errors);
    host.load(&[reader(1, false)], LoadOptions::default()).await.unwrap();
    let own = host.host_object("t.eager-read").unwrap();
    let consumer = hold(&host, &read(1), json!({}));
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!("one"));
    let error = reload(&host, reader(2, false)).await.unwrap_err();
    assert!(is(&error, "contract_mismatch"), "{error}");
    assert!(host.host_object("t.eager-read").unwrap().ptr_eq(&own));
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!("one"));
    reload(&host, reader(2, true)).await.unwrap();
    assert_eq!(host.host_object("t.eager-read").unwrap().contract().fingerprint(), read(2).fingerprint());
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!("two"));
    assert_eq!(consumer.contract().fingerprint(), read(1).fingerprint(), "only the bootstrap hold is retokened");
    let modern = hold(&host, &read(2), json!({}));
    assert_eq!(modern.call_json("fetch", Json::Null, None).await.unwrap(), json!("two"));
    assert!(errors.is_empty(), "{:?}", errors.all());
    host.close().await.unwrap();
}

fn consumer_facet(version: i64) -> Facet {
    let consumer = local_type("t.eager-consumer", object(&[]), vec![("read", method(None, string()).into())]);
    define_facet("consumer", move |api| {
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let reader = decl.get(&read(version), json!({}), Default::default())?;
            let member = if version == 1 { "read" } else { "fetch" };
            Ok(Implementation::new().method("read", move |_, ctx| {
                let reader = reader.clone();
                async move { reader.call_json(member, Json::Null, Some(&ctx)).await }
            }))
        };
        api.serve(&consumer, factory, Default::default())
    })
}

#[tokio::test]
async fn object_owned_old_version_references_block_an_upgrade_unless_their_consumer_is_replaced_too() {
    let _serial = serial().await;
    let host = quiet();
    host.load(&[reader(1, false), consumer_facet(1)], LoadOptions::default()).await.unwrap();
    let contract = local_type("t.eager-consumer", object(&[]), vec![("read", method(None, string()).into())]);
    let consumer = hold(&host, &contract, json!({}));
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!("one"));
    let error = reload(&host, reader(2, false)).await.unwrap_err();
    assert!(is(&error, "contract_mismatch"), "{error}");
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!("one"));
    let plugins = vec![reader(2, false).into(), consumer_facet(2).into()];
    host.reload(plugins, ReloadOptions::default()).await.unwrap();
    assert_eq!(consumer.call_json("read", Json::Null, None).await.unwrap(), json!("two"));
    assert_eq!(host.host_object("t.eager-read").unwrap().contract().fingerprint(), read(2).fingerprint());
    host.close().await.unwrap();
}

#[tokio::test]
async fn a_forgotten_bump_is_refused_even_when_the_only_consumer_is_the_host_reference() {
    let _serial = serial().await;
    let host = quiet();
    host.load(&[reader(1, false)], LoadOptions::default()).await.unwrap();
    let incompatible = local_type("t.eager-read", object(&[]), vec![("fetch", method(None, string()).into())]);
    let facet = define_facet("read", move |api| {
        let factory = |_: &Json, _: &Json, _: &mut Declare| {
            Ok(Implementation::new().method("fetch", |_, _| std::future::ready(Ok(json!("bad")))))
        };
        api.serve(&incompatible, factory, ServeOptions::host())
    });
    let error = reload(&host, facet).await.unwrap_err();
    assert!(is(&error, "contract_mismatch") && error.to_string().contains("without a version bump"), "{error}");
    assert_eq!(host.host_object("t.eager-read").unwrap().contract().fingerprint(), read(1).fingerprint());
    host.close().await.unwrap();
}

#[tokio::test]
async fn a_cluster_upgrade_retokens_host_references_only_after_every_host_prepared() {
    let _serial = serial().await;
    let log: Log = Log::default();
    let valid = Arc::new(AtomicBool::new(false));
    let server = quiet();
    let loader = Arc::new(InProcessLoader::<Facet>::new());
    let (module_log, module_valid) = (log.clone(), valid.clone());
    loader.define("switch", move |_| Ok(switch_facet(&module_log, 2, module_valid.load(Ordering::SeqCst))));
    let worker = create_host(HostOptions { loader: Some(loader), ..host_options("worker", &Log::default()) });
    server.load(&[switch_facet(&log, 1, true)], LoadOptions::default()).await.unwrap();
    worker.load(&[switch_facet(&log, 1, true)], LoadOptions::default()).await.unwrap();
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::host("worker", json!({"worker": true}))).unwrap();
    worker.connect(up, ConnectOptions::new(json!({"kind": "server"})));
    chord2_support::net::connected(&server, 1).await;
    tick().await;
    let server_own = server.host_object("t.eager-switch").unwrap();
    let worker_own = worker.host_object("t.eager-switch").unwrap();
    let remote = || ReloadOptions {
        remote: Some([("worker".to_string(), vec!["switch".to_string()])].into()),
        ..Default::default()
    };
    let error = server.reload(vec![switch_facet(&log, 2, true).into()], remote()).await.unwrap_err();
    assert!(is(&error, "invalid"), "{error}");
    assert!(server.host_object("t.eager-switch").unwrap().ptr_eq(&server_own));
    assert!(worker.host_object("t.eager-switch").unwrap().ptr_eq(&worker_own));
    valid.store(true, Ordering::SeqCst);
    server.reload(vec![switch_facet(&log, 2, true).into()], remote()).await.unwrap();
    until(|| worker.code_version() == server.code_version()).await;
    for host in [&server, &worker] {
        assert_eq!(host.host_object("t.eager-switch").unwrap().contract().fingerprint(), switch(2).fingerprint());
        assert_eq!(current(host), json!({"1": false}));
    }
    assert_eq!(log.all(), ["open", "open"]);
    worker.close().await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_reload_that_changes_a_lifetime_creates_the_host_object_or_lets_it_go() {
    let _serial = serial().await;
    let host = quiet();
    let kept = local_type("t.lifetime-change", object(&[]), vec![]);
    let log: Log = Log::default();
    let facet = |host_lifetime: bool| {
        let (log, kept) = (log.clone(), kept.clone());
        define_facet("kept", move |api| {
            let log = log.clone();
            let label = if host_lifetime { "host" } else { "referenced" };
            let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
                let (opens, closes) = (log.clone(), log.clone());
                let life = ResourceDeclaration::new(1, move |_| opens.push_ready(format!("open {label}")))
                    .with_close(move |_: ()| closes.push_ready("close"));
                decl.resource("life", life)?;
                Ok(Implementation::new())
            };
            let options = if host_lifetime { ServeOptions::host() } else { ServeOptions::default() };
            api.serve(&kept, factory, options)
        })
    };
    install(&host, facet(false)).await;
    assert!(log.is_empty(), "a referenced object waits for its first reference");
    reload(&host, facet(true)).await.unwrap();
    host.ready().await.unwrap();
    assert_eq!(log.all(), ["open host"], "created by the reload that made it a host object");
    reload(&host, facet(false)).await.unwrap();
    until(|| log.len() >= 2).await;
    assert_eq!(log.all(), ["open host", "close"], "nobody else references it: disposed");
    host.close().await.unwrap();
}
