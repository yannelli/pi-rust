//! Port of upstream `test/hydration.test.ts`: a local object goes live only with its state, also where it forwards
//! or derives from a remote reference's member; activation waits at most `ready_timeout` (a cycle fails with a hint).

mod chord2_support;

use std::sync::Arc;
use std::time::Duration;

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, GetOptions, Host, HostOptions, Implementation, Link, LinkOptions,
    LoadOptions, Ref, RemoteDef, ServeOptions, VariantDef, create_host, define_facet, host_named, memory_link_pair,
    method, remote,
};
use pi_chord::{ChordResult, Json, json};

fn read(reference: &Ref, member: &str) -> ChordResult<Json> {
    reference.state(member).expect("a state member").value()
}

fn remote_on(name: &str, host: &str, input: Json, main: VariantDef) -> Contract {
    let def = RemoteDef { version: 1, input, host: host_named(host).unwrap(), variants: vec![("main".into(), main)] };
    remote(name, &def).unwrap()
}

/// Lives on the worker `w`; its value is set by `set`.
fn source() -> Contract {
    remote_on(
        "t.e8-source",
        "w",
        object(&[]),
        VariantDef::new().state("v", string()).method("set", method(string(), None)),
    )
}

/// Lives on the server; forwards (and derives from) the worker's `Source`.
fn forward() -> Contract {
    let main = VariantDef::new().state("v", string()).state("size", number());
    remote_on("t.e8-forward", "server", object(&[req("how", string())]), main)
}

fn serve_source(host: &Host) {
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let v = decl.state("v", string(), json!("from the worker"))?;
        let cell = v.clone();
        Ok(Implementation::variant("main").state("v", v).method("set", move |text, ctx| {
            let cell = cell.clone();
            async move { cell.replace(&ctx, text).map(|()| Json::Null) }
        }))
    };
    host.serve(&source(), factory, ServeOptions::default()).unwrap();
}

struct Cluster {
    server: Host,
    worker: Host,
}

/// A server with a worker child `w` (accepted after `delay` ms, as p2's server admits a worker after it is ready).
fn cluster(delay: Option<u64>, ready_timeout: Option<Duration>) -> Cluster {
    let ready_timeout = ready_timeout.or(HostOptions::default().ready_timeout);
    let server = create_host(HostOptions { ready_timeout, ..host_options("server", &Log::default()) });
    let (down, up) = memory_link_pair();
    let upstream: Option<(Arc<dyn Link>, ConnectOptions)> = Some((up, ConnectOptions::new(json!({"kind": "server"}))));
    let worker = create_host(HostOptions { upstream, ready_timeout, ..host_options("w", &Log::default()) });
    let admitting = server.clone();
    let admit = move || admitting.accept(down, AcceptOptions::host("w", json!({"kind": "worker"}))).unwrap();
    match delay {
        None => admit(),
        Some(ms) => {
            tokio::spawn(async move {
                sleep_ms(ms).await;
                admit();
            });
        }
    }
    Cluster { server, worker }
}

impl Cluster {
    fn browser(&self) -> Host {
        let (down, up) = memory_link_pair();
        self.server.accept(down, AcceptOptions::client(json!({"user": "u"}))).unwrap();
        browser_on(&up, LinkOptions::default())
    }

    async fn close(self) {
        self.server.close().await.unwrap();
        self.worker.close().await.unwrap();
    }
}

#[tokio::test]
async fn an_object_that_forwards_and_derives_from_a_remote_member_goes_live_with_its_value() {
    let _serial = serial().await;
    let c = cluster(None, None);
    serve_source(&c.worker);
    // Held outside the factory (no `get` on the declaration): activation still waits for its snapshot.
    let held = c.server.hold(&source(), json!({}), GetOptions::default()).unwrap();
    let factory = move |input: &Json, _: &Json, decl: &mut Declare| {
        let source = match input["how"].as_str() {
            Some("declared") => decl.get(&source(), json!({}), GetOptions::default())?,
            _ => held.clone(),
        };
        let v = source.state("v").unwrap();
        let size = decl.derive(vec![v.clone()], |values| {
            Ok(Json::from(values[0].as_str().unwrap_or_default().encode_utf16().count() as f64))
        });
        Ok(Implementation::variant("main").state("v", v).state("size", size))
    };
    c.server.serve(&forward(), factory, ServeOptions::default()).unwrap();
    let browser = c.browser();
    for how in ["held", "declared"] {
        let forwarded = browser.hold(&forward(), json!({"how": how}), GetOptions::default()).unwrap();
        forwarded.ready().await.unwrap();
        assert_eq!(read(&forwarded, "v").unwrap(), json!("from the worker"));
        assert_eq!(read(&forwarded, "size").unwrap(), json!("from the worker".len()));
        let local = c.server.hold(&forward(), json!({"how": how}), GetOptions::default()).unwrap();
        local.ready().await.unwrap();
        assert_eq!(read(&local, "v").unwrap(), json!("from the worker"));
    }
    c.close().await;
}

#[tokio::test]
async fn a_member_read_inside_a_factory_throws_not_ready_and_is_a_plain_value_once_live() {
    let _serial = serial().await;
    let host = quiet();
    let inner = local_type("t.e8-inner", object(&[]), vec![("n", number().into())]);
    let outer = local_type("t.e8-outer", object(&[]), vec![("m", number().into())]);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        Ok(Implementation::new().state("n", decl.state("n", number(), json!(7))?))
    };
    host.serve(&inner, factory, ServeOptions::default()).unwrap();
    let seen: Log<Json> = Log::default();
    let (pushed, inner_type) = (seen.clone(), inner.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let inner = decl.get(&inner_type, json!({}), GetOptions::default())?;
        let n = inner.state("n").unwrap();
        pushed.push(match n.value() {
            Ok(value) => value,
            Err(error) if is(&error, "not_ready") => json!("not_ready"),
            Err(error) => json!(error.to_string()),
        });
        let m = decl.derive(vec![n], |values| Ok(Json::from(values[0].as_f64().unwrap_or_default() * 2.0)));
        Ok(Implementation::new().state("m", m))
    };
    host.serve(&outer, factory, ServeOptions::default()).unwrap();
    let outer_ref = host.hold(&outer, json!({}), GetOptions::default()).unwrap();
    outer_ref.ready().await.unwrap();
    assert_eq!(seen.all(), vec![json!("not_ready")]);
    assert_eq!(read(&outer_ref, "m").unwrap(), json!(14));
}

#[tokio::test]
async fn a_host_object_of_a_child_holding_the_parent_object_goes_live_once_the_parent_admits_the_child() {
    let _serial = serial().await;
    let settings = || remote_on("t.e8-settings", "server", object(&[]), VariantDef::new().state("theme", string()));
    let boot = local_type("t.e8-boot", object(&[]), vec![("theme", string().into())]);
    let c = cluster(Some(20), None);
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        Ok(Implementation::variant("main").state("theme", decl.state("theme", string(), json!("dark"))?))
    };
    c.server.serve(&settings(), factory, ServeOptions::default()).unwrap();
    let boot_type = boot.clone();
    let facet = define_facet("boot", move |api| {
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let settings = decl.get(&settings(), json!({}), GetOptions::default())?;
            Ok(Implementation::new().state("theme", settings.state("theme").unwrap()))
        };
        api.serve(&boot_type, factory, ServeOptions::host())
    });
    c.worker.install(&facet, LoadOptions { await_host_objects: false }).await.unwrap();
    c.worker.ready().await.unwrap();
    let boot_ref = c.worker.hold(&boot, json!({}), GetOptions::default()).unwrap();
    boot_ref.ready().await.unwrap();
    assert_eq!(read(&boot_ref, "theme").unwrap(), json!("dark"));
    c.close().await;
}

#[tokio::test]
async fn a_reference_cycle_across_hosts_fails_at_ready_timeout_with_a_cycle_hint() {
    let _serial = serial().await;
    let a = || remote_on("t.e8-cycle-a", "server", object(&[]), VariantDef::new());
    let b = || remote_on("t.e8-cycle-b", "w", object(&[]), VariantDef::new());
    let c = cluster(None, Some(Duration::from_millis(50)));
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        decl.get(&b(), json!({}), GetOptions::default())?;
        Ok(Implementation::variant("main"))
    };
    c.server.serve(&a(), factory, ServeOptions::default()).unwrap();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        decl.get(&a(), json!({}), GetOptions::default())?;
        Ok(Implementation::variant("main"))
    };
    c.worker.serve(&b(), factory, ServeOptions::default()).unwrap();
    let held = c.server.hold(&a(), json!({}), GetOptions::default()).unwrap();
    let error = held.ready().await.unwrap_err();
    let hinted = error.to_string().contains("cycle");
    assert!(is(&error, "unavailable") && error.reason() == Some("ready_timeout") && hinted, "{error}");
    sleep_ms(10).await;
    c.close().await;
}
