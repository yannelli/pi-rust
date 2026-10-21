//! Port of upstream `test/provider-cost.test.ts`: one change to K clients gives one state frame per link, with the
//! same ops on every link. Upstream also counts encoder calls (`vi.mock` on the delta module): those two
//! assertions are left out, since a Rust test cannot swap the runtime's encoder.

mod chord2_support;

use std::sync::{Arc, Mutex};

use chord2_support::net::*;
use chord2_support::*;
use pi_chord::host2::link::{CloseHandler, FrameHandler};
use pi_chord::host2::wire::{Frame, PeerKind, StateFrame};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, GetOptions, HostOptions, Implementation, Link, Received, Ref,
    RemoteDef, ServeOptions, VariantDef, create_host, host_named, memory_link_pair, method, remote,
};
use pi_chord::{Json, json};

/// A client's end of a link that records the frames it receives.
struct CountingLink {
    inner: Arc<dyn Link>,
    received: Arc<Mutex<Vec<Frame>>>,
}

impl CountingLink {
    fn received(&self) -> Vec<Frame> {
        self.received.lock().unwrap().clone()
    }
}

impl Link for CountingLink {
    fn closed(&self) -> bool {
        self.inner.closed()
    }

    fn send(&self, frame: &Frame) -> Option<usize> {
        self.inner.send(frame)
    }

    fn receive(&self, handler: FrameHandler) {
        let received = self.received.clone();
        self.inner.receive(Arc::new(move |message| {
            if let Received::Frame(frame) = &message {
                received.lock().unwrap().push(frame.clone());
            }
            handler(message)
        }));
    }

    fn on_close(&self, handler: CloseHandler) {
        self.inner.on_close(handler);
    }

    fn close(&self, reason: Option<&str>) {
        self.inner.close(reason);
    }
}

fn list() -> Contract {
    let main = VariantDef::new().state("items", array(string())).method("add", method(string(), None));
    let def = RemoteDef {
        version: 1,
        input: object(&[]),
        host: host_named("server").unwrap(),
        variants: vec![("main".into(), main)],
    };
    remote("t.e8-cost", &def).unwrap()
}

fn state_frames(frames: &[Frame]) -> Vec<StateFrame> {
    let state =
        frames.iter().filter_map(|frame| if let Frame::State(state) = frame { Some(state.clone()) } else { None });
    state.collect()
}

fn items(reference: &Ref) -> Json {
    reference.state("items").and_then(|items| items.value().ok()).unwrap_or_default()
}

#[tokio::test]
async fn one_change_to_k_clients_gives_one_frame_per_link_with_the_same_ops_on_every_link() {
    let _serial = serial().await;
    let server = named("server");
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let items = decl.state("items", array(string()), json!([]))?;
        let adder = items.clone();
        Ok(Implementation::variant("main").state("items", items).method("add", move |text, ctx| {
            let items = adder.clone();
            async move {
                items.change(&ctx, |draft| draft.push(text))?;
                Ok(Json::Null)
            }
        }))
    };
    server.serve(&list(), factory, ServeOptions::default()).unwrap();
    let clients: Vec<_> = (0..4)
        .map(|i| {
            let (down, up) = memory_link_pair();
            // One identity: every client reaches the same object.
            server.accept(down, AcceptOptions::client(json!({"user": "u"}))).unwrap();
            let link = Arc::new(CountingLink { inner: up, received: Arc::default() });
            let upstream = Some((link.clone() as Arc<dyn Link>, ConnectOptions::new(json!({"kind": "server"}))));
            let options = host_options(&format!("browser{i}"), &Log::default());
            let host = create_host(HostOptions { kind: PeerKind::Client, upstream, ..options });
            let reference = host.hold(&list(), json!({}), GetOptions::default()).unwrap();
            (host, link, reference)
        })
        .collect();
    for (_, _, reference) in &clients {
        reference.ready().await.unwrap();
    }
    let before: Vec<usize> = clients.iter().map(|(_, link, _)| state_frames(&link.received()).len()).collect();
    call(&clients[0].2, "add", json!("x")).await.unwrap();
    sleep_ms(30).await;
    let frames: Vec<Vec<StateFrame>> = clients
        .iter()
        .zip(&before)
        .map(|((_, link, _), before)| state_frames(&link.received())[*before..].to_vec())
        .collect();
    for list in &frames {
        assert_eq!(list.len(), 1);
    }
    let ops: std::collections::HashSet<String> = frames.iter().map(|list| format!("{:?}", list[0].ops)).collect();
    assert_eq!(ops.len(), 1, "every link carries the same ops");
    for (_, _, reference) in &clients {
        assert_eq!(items(reference), json!(["x"]));
    }
    server.close().await.unwrap();
}
