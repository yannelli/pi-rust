//! Port of upstream `test/variants.test.ts`: a re-hold after a link loss. The "variants" case is left out: it
//! switches the served variant with `server.reload`, and reloads come in phase 4.

mod chord2_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::{Frame, StateKind};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, GetOptions, Implementation, Link, LinkOptions, LoadOptions,
    Readable, Received, Ref, RefStatus, RemoteDef, ServeOptions, VariantDef, define_facet, host_named,
    memory_link_pair, method, remote,
};
use pi_chord::{ChordResult, Json, json};

/// A link that records the frames crossing it, per direction (upstream `CountingLink`).
struct Counting {
    inner: Memory,
    sent: Log<Frame>,
    received: Log<Frame>,
}

impl Link for Counting {
    fn closed(&self) -> bool {
        self.inner.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        self.sent.push(frame.clone());
        self.inner.send(frame)
    }

    fn receive(&self, handler: FrameHandler) {
        let received = self.received.clone();
        self.inner.receive(Arc::new(move |message| {
            if let Received::Frame(frame) = &message {
                received.push(frame.clone());
            }
            handler(message);
        }));
    }

    fn on_close(&self, handler: CloseHandler) {
        self.inner.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        self.inner.close(reason);
    }
}

/// Polls `check` every 2 ms, for at most about 2 s (upstream `until`).
async fn soon(check: impl Fn() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        sleep_ms(2).await;
    }
    assert!(check(), "condition not reached");
}

fn read(reference: &Ref, member: &str) -> ChordResult<Json> {
    reference.state(member).expect("a state member").value()
}

fn doc() -> Contract {
    let bump = || method(None, None);
    let a = VariantDef::new().state("x", number()).method("bump", bump());
    let b = VariantDef::new().state("x", number()).state("y", string()).method("bump", bump());
    let c = VariantDef::new().state("y", string()).method("bump", bump());
    let variants = vec![("a".into(), a), ("b".into(), b), ("c".into(), c)];
    let host = host_named("server").unwrap();
    remote("t.e8-doc", &RemoteDef { version: 1, input: object(&[]), host, variants }).unwrap()
}

/// The server's facet: `Doc` in variant `variant` (the cells survive reloads by name).
fn doc_facet(variant: &'static str) -> pi_chord::host2::Facet {
    define_facet("doc", move |api| {
        let factory = move |_: &Json, _: &Json, decl: &mut Declare| {
            let x = decl.state("x", number(), json!(1))?;
            let y = decl.state("y", string(), json!("why"))?;
            let cell = x.clone();
            let mut implementation = Implementation::variant(variant).method("bump", move |_, ctx| {
                let cell = cell.clone();
                async move {
                    let next = cell.value()?.as_f64().unwrap_or_default() + 1.0;
                    cell.replace(&ctx, json!(next)).map(|()| Json::Null)
                }
            });
            if variant != "c" {
                implementation = implementation.state("x", x);
            }
            if variant != "a" {
                implementation = implementation.state("y", y);
            }
            Ok(implementation)
        };
        api.serve(&doc(), factory, ServeOptions::default())
    })
}

#[tokio::test]
async fn keeps_the_last_values_readable_sends_one_fresh_unit_and_a_reference_joining_meanwhile_waits_for_it() {
    let _serial = serial().await;
    let server = named("server");
    server.install(&doc_facet("b"), LoadOptions::default()).await.unwrap();
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"user": "u"}))).unwrap();
    let browser = browser_on(&up, LinkOptions::default());
    let doc_ref = browser.hold(&doc(), json!({}), GetOptions::default()).unwrap();
    doc_ref.ready().await.unwrap();
    call(&doc_ref, "bump", Json::Null).await.unwrap();
    soon(|| read(&doc_ref, "x").ok() == Some(json!(2))).await;
    up.close(None);
    soon(|| doc_ref.status() == RefStatus::Loading).await;
    // The last values stay readable while the binding re-holds.
    assert_eq!(read(&doc_ref, "x").unwrap(), json!(2));
    let joining = browser.hold(&doc(), json!({}), GetOptions::default()).unwrap();
    assert_eq!(joining.status(), RefStatus::Loading);
    let joined = Arc::new(AtomicBool::new(false));
    let (flag, ready) = (joined.clone(), joining.ready());
    tokio::spawn(async move {
        if ready.await.is_ok() {
            flag.store(true, Ordering::SeqCst);
        }
    });
    sleep_ms(20).await;
    assert!(!joined.load(Ordering::SeqCst), "a reference joining during the re-hold waits for the fresh unit");

    let (down2, up2) = memory_link_pair();
    server.accept(down2, AcceptOptions::client(json!({"user": "u"}))).unwrap();
    let link = Arc::new(Counting { inner: up2, sent: Log::default(), received: Log::default() });
    browser.connect(link.clone(), ConnectOptions::new(json!({"kind": "server"})));
    joining.ready().await.unwrap();
    assert_eq!(doc_ref.status(), RefStatus::Live);
    let sent = link.sent.all();
    let holds: Vec<bool> =
        sent.iter().filter_map(|frame| if let Frame::Hold(hold) = frame { Some(hold.again) } else { None }).collect();
    assert_eq!(holds.first(), Some(&true), "a re-hold says again");
    assert_eq!(holds.len(), 1, "one hold for both references");
    let received = link.received.all();
    let mut snaps: Vec<String> = received
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::State(state) if state.kind == StateKind::Snap => Some(state.m),
            _ => None,
        })
        .collect();
    snaps.sort();
    assert_eq!(snaps, ["x", "y"], "one snapshot per member");
    // The old hold was released with the link: the server made a fresh object (its cells start over).
    assert_eq!(read(&doc_ref, "x").unwrap(), json!(1));
    browser.close().await.unwrap();
    server.close().await.unwrap();
}
