//! Port of upstream `test/readiness.test.ts`. Left out: deferring eager readiness until a parent host admits a
//! worker (links, phase 3).

mod chord2_support;

use std::future::ready;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chord2_support::*;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::{
    Contract, Declare, Facet, GetOptions, Host, Implementation, LoadOptions, RefStatus, RegistryView, ServeOptions,
    define_facet, define_registry, forbidden,
};
use pi_chord::{BoxFuture, ChordError, ChordResult, Json, json};
use tokio::sync::oneshot;

fn eager() -> Contract {
    local_type("ready.eager", object(&[]), vec![])
}

fn lazy() -> Contract {
    local_type("ready.lazy", object(&[]), vec![])
}

#[derive(Clone, Copy)]
enum Operation {
    Install,
    Load,
}

fn start(host: &Host, operation: Operation, facet: Facet) -> BoxFuture<ChordResult<()>> {
    match operation {
        Operation::Install => host.install(&facet, LoadOptions::default()),
        Operation::Load => host.load(&[facet], LoadOptions::default()),
    }
}

/// The same error: upstream compares by identity, here by code and message.
fn assert_same(error: &ChordError, cause: &ChordError) {
    assert_eq!((error.code(), error.to_string()), (cause.code(), cause.to_string()));
}

fn plain(_: &Json, _: &Json, _: &mut Declare) -> ChordResult<Implementation> {
    Ok(Implementation::new())
}

/// Startup must not report ready while an eager object has failed.
async fn awaits_resource_activation_and_registration(operation: Operation) {
    let _serial = serial().await;
    let errors = Log::default();
    let host = quiet_with(&errors);
    let entries = define_registry::<String>("ready.entries", None);
    let gate: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::default();
    let lazy_runs = Arc::new(AtomicUsize::new(0));
    let (registry, opener, runs) = (entries.clone(), gate.clone(), lazy_runs.clone());
    let facet = define_facet("ready", move |api| {
        let (registry, opener, runs) = (registry.clone(), opener.clone(), runs.clone());
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let opener = opener.clone();
            let open = move |_: OpenOptions| {
                let (sender, receiver) = oneshot::channel();
                *opener.lock().unwrap() = Some(sender);
                async move { receiver.await.map_err(|_| ChordError::error("never opened")) }
            };
            decl.resource("r", ResourceDeclaration::new(1, open))?;
            decl.register(&registry, [("eager".to_string(), "live".to_string())])?;
            Ok(Implementation::new())
        };
        api.serve(&eager(), factory, ServeOptions::host())?;
        let count = move |_: &Json, _: &Json, _: &mut Declare| {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(Implementation::new())
        };
        api.serve(&lazy(), count, ServeOptions::default())
    });
    let loading = tokio::spawn(start(&host, operation, facet));
    tick().await;
    assert!(!loading.is_finished());
    assert_eq!(host.registry(&entries).get("eager", None).unwrap(), None);
    let sender = gate.lock().unwrap().take();
    sender.expect("the resource is opening").send(()).unwrap();
    loading.await.unwrap().unwrap();
    assert_eq!(host.registry(&entries).get("eager", None).unwrap().as_deref(), Some("live"));
    assert_eq!(lazy_runs.load(Ordering::SeqCst), 0);
    host.close().await.unwrap();
    assert!(errors.is_empty(), "{:?}", errors.all());
}

async fn rejects_eager_failure_with_the_activation_error(operation: Operation, failure: &'static str) {
    let _serial = serial().await;
    let errors = Log::default();
    let host = quiet_with(&errors);
    let cause = forbidden(Some(&format!("{failure} failed")));
    let thrown = cause.clone();
    let facet = define_facet("bad", move |api| {
        let refused = thrown.clone();
        api.serve(&lazy(), move |_, _, _| Err(refused.clone()), ServeOptions::default())?;
        let thrown = thrown.clone();
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            match failure {
                "factory" => return Err(thrown.clone()),
                "dependency" => drop(decl.get(&lazy(), json!({}), GetOptions::default())?),
                _ => {
                    let thrown = thrown.clone();
                    let open = move |_: OpenOptions| ready(Err::<(), _>(thrown.clone()));
                    decl.resource("r", ResourceDeclaration::new(1, open))?;
                }
            }
            Ok(Implementation::new())
        };
        api.serve(&eager(), factory, ServeOptions::host())
    });
    assert_same(&start(&host, operation, facet).await.unwrap_err(), &cause);
    assert_same(&host.ready().await.unwrap_err(), &cause);
    let errors = errors.all();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_same(&errors[0], &cause);
    host.close().await.unwrap();
}

#[tokio::test]
async fn install_awaits_resource_activation_and_registration_leaving_lazy_objects_lazy() {
    awaits_resource_activation_and_registration(Operation::Install).await;
}

#[tokio::test]
async fn load_awaits_resource_activation_and_registration_leaving_lazy_objects_lazy() {
    awaits_resource_activation_and_registration(Operation::Load).await;
}

#[tokio::test]
async fn install_rejects_eager_factory_failure_with_the_activation_error() {
    rejects_eager_failure_with_the_activation_error(Operation::Install, "factory").await;
}

#[tokio::test]
async fn install_rejects_eager_dependency_failure_with_the_activation_error() {
    rejects_eager_failure_with_the_activation_error(Operation::Install, "dependency").await;
}

#[tokio::test]
async fn install_rejects_eager_resource_failure_with_the_activation_error() {
    rejects_eager_failure_with_the_activation_error(Operation::Install, "resource").await;
}

#[tokio::test]
async fn load_rejects_eager_factory_failure_with_the_activation_error() {
    rejects_eager_failure_with_the_activation_error(Operation::Load, "factory").await;
}

#[tokio::test]
async fn load_rejects_eager_dependency_failure_with_the_activation_error() {
    rejects_eager_failure_with_the_activation_error(Operation::Load, "dependency").await;
}

#[tokio::test]
async fn load_rejects_eager_resource_failure_with_the_activation_error() {
    rejects_eager_failure_with_the_activation_error(Operation::Load, "resource").await;
}

fn consumer(_: &Json, _: &Json, decl: &mut Declare) -> ChordResult<Implementation> {
    decl.get(&lazy(), json!({}), GetOptions::default())?;
    Ok(Implementation::new())
}

#[tokio::test]
async fn load_installs_every_facet_before_awaiting_eager_consumers() {
    let _serial = serial().await;
    let errors = Log::default();
    let host = quiet_with(&errors);
    let facets = [
        define_facet("consumer", |api| api.serve(&eager(), consumer, ServeOptions::host())),
        define_facet("provider", |api| api.serve(&lazy(), plain, ServeOptions::default())),
    ];
    host.load(&facets, LoadOptions::default()).await.unwrap();
    assert_eq!(host.hold(&eager(), json!({}), GetOptions::default()).unwrap().status(), RefStatus::Live);
    host.close().await.unwrap();
    assert!(errors.is_empty(), "{:?}", errors.all());
}

#[tokio::test]
async fn missing_eager_dependencies_reject_load_instead_of_only_reaching_on_error() {
    let _serial = serial().await;
    let host = quiet();
    let facets = [define_facet("missing", |api| api.serve(&eager(), consumer, ServeOptions::host()))];
    let error = host.load(&facets, LoadOptions::default()).await.unwrap_err();
    assert!(is(&error, "unavailable") && error.reason() == Some("unknown_type"), "{error}");
    host.close().await.unwrap();
}

#[tokio::test]
async fn ready_includes_direct_host_serves_but_not_later_serves() {
    let _serial = serial().await;
    let host = quiet();
    let first = local_type("ready.first", object(&[]), vec![]);
    let created = Arc::new(AtomicUsize::new(0));
    let counted = created.clone();
    let count = move |_: &Json, _: &Json, _: &mut Declare| {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(Implementation::new())
    };
    host.serve(&first, count, ServeOptions::host()).unwrap();
    let first_ready = host.ready();
    host.serve(&eager(), |_, _, _| Err(forbidden(Some("later"))), ServeOptions::host()).unwrap();
    first_ready.await.unwrap();
    assert_eq!(created.load(Ordering::SeqCst), 1);
    let error = host.ready().await.unwrap_err();
    assert!(error.to_string().contains("later"), "{error}");
    host.close().await.unwrap();
}
