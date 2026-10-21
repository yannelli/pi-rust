//! Port of upstream `test/awaiting.test.ts` (awaited gets), adapted to one host: upstream holds `Slow` from a
//! browser over a link and counts `hold` and `release` frames. Here the object's gate resource shows the same:
//! its `open` sees the abort once the last reference is released (or at disposal), and it closes on disposal.

mod chord2_support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chord2_support::*;
use pi_chord::context::AbortController;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::{
    Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation, RemoteDef, RootOptions, ServeOptions,
    VariantDef, create_host, host_named, remote,
};
use pi_chord::{BoxFuture, ChordResult, Json, json};

fn slow() -> Contract {
    let variants = vec![("main".to_string(), VariantDef::new().state("n", number()))];
    let def = RemoteDef { version: 1, input: object(&[]), host: host_named("server").unwrap(), variants };
    remote("t.e8-slow", &def).unwrap()
}

/// A host whose `Slow` object goes live once `opened` is opened; `log` records the gate's abort and close.
fn setup(ready_timeout: Option<Duration>) -> (Host, Latch, Log) {
    let opened = Latch::default();
    let log: Log = Log::default();
    let host = create_host(HostOptions {
        name: Some("server".into()),
        principal: json!({"kind": "host"}),
        on_error: Some(Arc::new(|_| {})),
        ready_timeout,
        ..HostOptions::default()
    });
    let (gate, logged) = (opened.clone(), log.clone());
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let (gate, aborts, closes) = (gate.clone(), logged.clone(), logged.clone());
        let open = move |options: OpenOptions| {
            let aborts = aborts.clone();
            options.signal.add_listener(move |_| aborts.push("abort"));
            let opened = gate.wait();
            async move {
                opened.await;
                Ok(())
            }
        };
        let close = move |()| {
            closes.push("close");
            std::future::ready(Ok(()))
        };
        decl.resource("gate", ResourceDeclaration::new(1, open).with_close(close))?;
        Ok(Implementation::variant("main").state("n", decl.state("n", number(), json!(1))?))
    };
    host.serve(&slow(), factory, ServeOptions::default()).unwrap();
    (host, opened, log)
}

/// Polls every 2 ms, at most 200 times (upstream's `until` in this file).
async fn until_slept(check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        sleep_ms(2).await;
    }
    assert!(check(), "condition not reached");
}

#[tokio::test]
async fn an_aborted_get_rejects_and_releases_only_its_reference() {
    let _serial = serial().await;
    let (host, opened, log) = setup(None);
    let abort = AbortController::new();
    let options = GetOptions { signal: Some(abort.signal()), ..GetOptions::default() };
    let first = tokio::spawn(host.get(&slow(), json!({}), options));
    let second = tokio::spawn(host.get(&slow(), json!({}), GetOptions::default()));
    sleep_ms(5).await;
    abort.abort(None);
    let error = first.await.unwrap().unwrap_err();
    assert!(is(&error, "aborted"), "{error}");
    sleep_ms(5).await;
    assert!(log.is_empty(), "the other awaiting get keeps the hold: {:?}", log.all());
    opened.open(Json::Null);
    let reference = second.await.unwrap().unwrap();
    assert_eq!(reference.state("n").unwrap().value().unwrap(), json!(1));
    assert_eq!(host.addresses().len(), 1);
    reference.release();
    until_slept(|| log.all().iter().any(|entry| entry == "close")).await;
    host.close().await.unwrap();
}

#[tokio::test]
async fn a_ctx_get_still_waiting_when_its_call_settles_rejects_aborted_and_releases_its_reference() {
    let _serial = serial().await;
    let (host, opened, log) = setup(None);
    let pending: Arc<Mutex<Option<BoxFuture<ChordResult<pi_chord::host2::Ref>>>>> = Arc::default();
    let slot = pending.clone();
    let run = move |ctx: Ctx| async move {
        *slot.lock().unwrap() = Some(ctx.get(&slow(), json!({}), GetOptions::default()));
        Ok(())
    };
    host.root(run, RootOptions::default()).await.unwrap();
    let pending = pending.lock().unwrap().take().unwrap();
    let error = pending.await.unwrap_err();
    assert!(is(&error, "aborted"), "{error}");
    // The root settled before the activation started: the object goes away without opening its gate.
    until_slept(|| host.addresses().is_empty()).await;
    assert!(log.is_empty(), "{:?}", log.all());
    opened.open(Json::Null);
    host.close().await.unwrap();
}

#[tokio::test]
async fn a_get_times_out_after_ready_timeout_with_ready_timeout_and_releases_its_hold() {
    let _serial = serial().await;
    let (host, _opened, log) = setup(Some(Duration::from_millis(30)));
    let error = host.get(&slow(), json!({}), GetOptions::default()).await.unwrap_err();
    assert!(is(&error, "unavailable") && error.reason() == Some("ready_timeout"), "{error}");
    until_slept(|| log.all().iter().any(|entry| entry == "abort")).await;
    host.close().await.unwrap();
}

#[tokio::test]
async fn ctx_get_resolves_hydrated_and_the_call_reference_is_released_when_the_call_ends() {
    let _serial = serial().await;
    let (host, opened, log) = setup(None);
    opened.open(Json::Null);
    let run = |ctx: Ctx| async move {
        let slow = ctx.get(&slow(), json!({}), GetOptions::default()).await?;
        slow.state("n").map_or(Ok(Json::Null), |n| n.value())
    };
    assert_eq!(host.root(run, RootOptions::default()).await.unwrap(), json!(1));
    until_slept(|| log.all().iter().any(|entry| entry == "close")).await;
    host.close().await.unwrap();
}
