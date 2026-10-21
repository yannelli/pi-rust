//! Port of upstream `test/awaiting.test.ts` in its own form: a browser awaits `Slow` from the server over a link
//! that counts the `hold` and `release` frames it sends. The one-host form is in `chord2_awaiting.rs`.

mod chord2_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chord2_support::net::Memory;
use chord2_support::*;
use pi_chord::context::AbortController;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::{Frame, PeerKind};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Ctx, Declare, GetOptions, Host, HostOptions, Implementation, Link, Ref,
    RemoteDef, RootOptions, ServeOptions, VariantDef, create_host, host_named, memory_link_pair, remote,
};

use pi_chord::{BoxFuture, ChordResult, Json, json};

fn slow() -> Contract {
    let variants = vec![("main".to_string(), VariantDef::new().state("n", number()))];
    let def = RemoteDef { version: 1, input: object(&[]), host: host_named("server").unwrap(), variants };
    remote("t.e8-slow", &def).unwrap()
}

/// A memory link that counts the holds and releases it sends.
#[derive(Clone, Default)]
struct Counts {
    holds: Arc<AtomicUsize>,
    releases: Arc<AtomicUsize>,
}

impl Counts {
    fn holds(&self) -> usize {
        self.holds.load(Ordering::SeqCst)
    }

    fn releases(&self) -> usize {
        self.releases.load(Ordering::SeqCst)
    }
}

struct CountingLink(Memory, Counts);

impl Link for CountingLink {
    fn closed(&self) -> bool {
        self.0.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        match frame {
            Frame::Hold(_) => self.1.holds.fetch_add(1, Ordering::SeqCst),
            Frame::Release { .. } => self.1.releases.fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
        self.0.send(frame)
    }

    fn receive(&self, handler: FrameHandler) {
        self.0.receive(handler);
    }

    fn on_close(&self, handler: CloseHandler) {
        self.0.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        Link::close(self.0.as_ref(), reason);
    }
}

struct Setup {
    server: Host,
    browser: Host,
    counts: Counts,
    opened: Latch,
}

/// A server whose `Slow` object goes live once `opened` is opened; a browser whose link counts frames.
fn setup(ready_timeout: Option<Duration>) -> Setup {
    let opened = Latch::default();
    let server = create_host(HostOptions {
        name: Some("server".into()),
        principal: json!({"kind": "host"}),
        on_error: Some(Arc::new(|_| {})),
        ..HostOptions::default()
    });
    let gate = opened.clone();
    let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let gate = gate.clone();
        let open = move |_: OpenOptions| {
            let opened = gate.wait();
            async move {
                opened.await;
                Ok(())
            }
        };
        decl.resource("gate", ResourceDeclaration::new(1, open))?;
        Ok(Implementation::variant("main").state("n", decl.state("n", number(), json!(1))?))
    };
    server.serve(&slow(), factory, ServeOptions::default()).unwrap();
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"user": "u"}))).unwrap();
    let counts = Counts::default();
    let link: Arc<dyn Link> = Arc::new(CountingLink(up, counts.clone()));
    let browser = create_host(HostOptions {
        name: Some("browser".into()),
        principal: json!({"kind": "host"}),
        kind: PeerKind::Client,
        upstream: Some((link, ConnectOptions::new(json!({"kind": "server"})))),
        on_error: Some(Arc::new(|_| {})),
        ready_timeout,
        ..HostOptions::default()
    });
    Setup { server, browser, counts, opened }
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
async fn an_aborted_get_rejects_and_releases_only_its_reference_and_the_binding_stays_for_the_others() {
    let _serial = serial().await;
    let Setup { server, browser, counts, opened } = setup(None);
    let abort = AbortController::new();
    let options = GetOptions { signal: Some(abort.signal()), ..GetOptions::default() };
    let first = tokio::spawn(browser.get(&slow(), json!({}), options));
    let second = tokio::spawn(browser.get(&slow(), json!({}), GetOptions::default()));
    sleep_ms(5).await;
    abort.abort(None);
    let error = first.await.unwrap().unwrap_err();
    assert!(is(&error, "aborted"), "{error}");
    sleep_ms(5).await;
    assert_eq!(counts.releases(), 0, "the other awaiting get keeps the hold");
    opened.open(Json::Null);
    let reference = second.await.unwrap().unwrap();
    assert_eq!(reference.state("n").unwrap().value().unwrap(), json!(1));
    assert_eq!(counts.holds(), 1);
    reference.release();
    until_slept(|| counts.releases() == 1).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_ctx_get_still_waiting_when_its_call_settles_rejects_aborted_and_releases_its_reference() {
    let _serial = serial().await;
    let Setup { server, browser, counts, opened } = setup(None);
    let pending: Arc<Mutex<Option<BoxFuture<ChordResult<Ref>>>>> = Arc::default();
    let slot = pending.clone();
    let run = move |ctx: Ctx| async move {
        *slot.lock().unwrap() = Some(ctx.get(&slow(), json!({}), GetOptions::default()));
        Ok(())
    };
    browser.root(run, RootOptions::default()).await.unwrap();
    let pending = pending.lock().unwrap().take().unwrap();
    let error = pending.await.unwrap_err();
    assert!(is(&error, "aborted"), "{error}");
    // The release can reach the link task before the hold went out: then neither is sent (upstream sends both).
    sleep_ms(10).await;
    assert_eq!(counts.releases(), counts.holds());
    let served = server.connections().await.iter().map(|info| info.served.len()).sum::<usize>();
    assert_eq!(served, 0);
    opened.open(Json::Null);
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_get_times_out_after_ready_timeout_with_ready_timeout_and_releases_its_hold() {
    let _serial = serial().await;
    let Setup { server, browser, counts, .. } = setup(Some(Duration::from_millis(30)));
    let error = browser.get(&slow(), json!({}), GetOptions::default()).await.unwrap_err();
    assert!(is(&error, "unavailable") && error.reason() == Some("ready_timeout"), "{error}");
    until_slept(|| counts.releases() == 1).await;
    server.close().await.unwrap();
}

#[tokio::test]
async fn ctx_get_resolves_hydrated_and_the_call_reference_is_released_when_the_call_ends() {
    let _serial = serial().await;
    let Setup { server, browser, counts, opened } = setup(None);
    opened.open(Json::Null);
    let run = |ctx: Ctx| async move {
        let slow = ctx.get(&slow(), json!({}), GetOptions::default()).await?;
        slow.state("n").map_or(Ok(Json::Null), |n| n.value())
    };
    assert_eq!(browser.root(run, RootOptions::default()).await.unwrap(), json!(1));
    until_slept(|| counts.releases() == 1).await;
    server.close().await.unwrap();
}
