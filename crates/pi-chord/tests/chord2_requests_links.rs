//! Port of upstream `test/requests.test.ts`, link cases: the link half of #1, #8, #9, and #10 with the server
//! across a link. The in-host cases are in `chord2_requests.rs`; #3, #5 and #7 need reloads (phase 4).

mod chord2_support;

use std::collections::HashSet;
use std::future::ready;
use std::sync::{Arc, Mutex};

use chord2_support::net::{browser_on, child_on, named};
use chord2_support::*;
use pi_chord::host2::declare::{OpenOptions, ResourceDeclaration};
use pi_chord::host2::{
    AcceptOptions, ConnectOptions, Contract, Declare, GetOptions, Host, Implementation, LinkOptions, PrincipalMatch,
    Ref, RefStatus, RemoteDef, ServeOptions, VariantDef, host_named, memory_link_pair, method, on_value, opaque,
    remote,
};
use pi_chord::{Json, json};

fn hold(host: &Host, contract: &Contract) -> Ref {
    host.hold(contract, json!({}), GetOptions::default()).unwrap()
}

fn on_server(name: &str, variant: &str, members: VariantDef) -> Contract {
    let variants = vec![(variant.to_string(), members)];
    let def = RemoteDef { version: 1, input: object(&[]), host: host_named("server").unwrap(), variants };
    remote(name, &def).unwrap()
}

/// A browser of `server`, accepted as `{guest: "g"}`.
fn browser_of(server: &Host) -> Host {
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    browser_on(&up, LinkOptions::default())
}

fn value_of(reference: &Ref, member: &str) -> Option<Json> {
    reference.state(member).and_then(|state| state.value().ok())
}

#[tokio::test]
async fn r1_a_returned_state_member_reaches_a_browser_with_its_snapshot_and_ops() {
    let _serial = serial().await;
    let view = || object(&[req("text", string())]);
    let conv =
        on_server("t.conv", "main", VariantDef::new().state("view", view()).method("set", method(string(), None)));
    let chat = on_server("t.chat", "viewer", VariantDef::new().state("view", view()));
    let server = named("server");
    let conv_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let state = decl.state("view", view(), json!({"text": ""}))?;
        Ok(Implementation::variant("main")
            .state("view", state.clone())
            .method("set", move |text, ctx| ready(state.replace(&ctx, json!({"text": text})).map(|()| Json::Null))))
    };
    server.serve(&conv, conv_factory, ServeOptions::default()).unwrap();
    let held = conv.clone();
    let chat_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let conv = decl.get(&held, json!({}), GetOptions::default())?;
        Ok(Implementation::variant("viewer").state("view", conv.state("view").unwrap()))
    };
    server.serve(&chat, chat_factory, ServeOptions::default()).unwrap();
    let browser = browser_of(&server);
    let remote_chat = hold(&browser, &chat);
    remote_chat.ready().await.unwrap();
    hold(&server, &conv).call("set", Some(json!("there")), None).await.unwrap();
    until(|| value_of(&remote_chat, "view") == Some(json!({"text": "there"}))).await;
}

#[tokio::test]
async fn r8_a_held_remote_reference_keeps_its_state_without_listeners() {
    let _serial = serial().await;
    let members = VariantDef::new().state("n", number()).method("set", method(number(), None));
    let counter = on_server("t.count8", "main", members);
    let server = named("server");
    let factory = |_: &Json, _: &Json, decl: &mut Declare| {
        let n = decl.state("n", number(), json!(0))?;
        Ok(Implementation::variant("main")
            .state("n", n.clone())
            .method("set", move |value, ctx| ready(n.replace(&ctx, value).map(|()| Json::Null))))
    };
    server.serve(&counter, factory, ServeOptions::default()).unwrap();
    let browser = browser_of(&server);
    let counter = hold(&browser, &counter);
    counter.ready().await.unwrap();
    assert_eq!(value_of(&counter, "n"), Some(json!(0)));
    let n = counter.state("n").unwrap();
    let stop = on_value(n.as_ref(), |_| {});
    counter.call("set", Some(json!(1)), None).await.unwrap();
    until(|| value_of(&counter, "n") == Some(json!(1))).await;
    stop.unsubscribe();
    counter.call("set", Some(json!(2)), None).await.unwrap();
    until(|| value_of(&counter, "n") == Some(json!(2))).await;
    let seen: Log<Json> = Log::default();
    let pushed = seen.clone();
    let again = on_value(n.as_ref(), move |value| pushed.push(value.clone()));
    assert_eq!(seen.all(), vec![json!(2)]);
    again.unsubscribe();
}

#[tokio::test]
async fn r9_references_report_status_changes_live_loading_gone_released() {
    let _serial = serial().await;
    let thing = on_server("t.thing9", "main", VariantDef::new());
    let server = named("server");
    server
        .serve(
            &thing,
            |_: &Json, _: &Json, _: &mut Declare| Ok(Implementation::variant("main")),
            ServeOptions::default(),
        )
        .unwrap();
    let options = GetOptions { principal: Some(json!({"user": "u"})), ..GetOptions::default() };
    let local = server.hold(&thing, json!({}), options).unwrap();
    let local_status: Log<Json> = Log::default();
    let pushed = local_status.clone();
    let _local = local.on_status(Arc::new(move |status, error| match status {
        RefStatus::Gone => pushed.push(json!([status.as_str(), error.and_then(code_of)])),
        _ => pushed.push(json!(status.as_str())),
    }));
    local.ready().await.unwrap();
    tick().await;
    server.invalidate_principal(PrincipalMatch::Pattern(json!({"user": "u"}))).await.unwrap();
    tick().await;
    assert_eq!(local_status.all(), vec![json!("live"), json!(["gone", "gone"])]);

    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    let browser = browser_on(&up, LinkOptions::default());
    let remote_thing = hold(&browser, &thing);
    let remote_status: Log = Log::default();
    let pushed = remote_status.clone();
    let _remote = remote_thing.on_status(Arc::new(move |status, _| pushed.push(status.as_str())));
    remote_thing.ready().await.unwrap();
    pi_chord::host2::Link::close(up.as_ref(), None);
    until(|| remote_status.len() == 2).await;
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::client(json!({"guest": "g"}))).unwrap();
    browser.connect(up, ConnectOptions::new(json!({"kind": "server"})));
    until(|| remote_status.len() == 3).await;
    remote_thing.release();
    tick().await;
    assert_eq!(remote_status.all(), ["live", "loading", "live", "released"]);
}

type Tracker = Arc<Mutex<HashSet<String>>>;

#[tokio::test]
async fn r10_host_close_disposes_holders_before_what_they_hold_and_before_closing_links() {
    let _serial = serial().await;
    let log: Log = Log::default();
    let upstream = on_server("t.remote10", "main", VariantDef::new().method("note", method(string(), None)));
    let server = named("server");
    let noted = log.clone();
    let note_factory = move |_: &Json, _: &Json, _: &mut Declare| {
        let noted = noted.clone();
        Ok(Implementation::variant("main").method("note", move |text, _| {
            noted.push(format!("server: {}", text.as_str().unwrap_or_default()));
            ready(Ok(Json::Null))
        }))
    };
    server.serve(&upstream, note_factory, ServeOptions::default()).unwrap();
    let (down, up) = memory_link_pair();
    server.accept(down, AcceptOptions::host("w", json!({"worker": "w"}))).unwrap();
    let errors: Log<pi_chord::ChordError> = Log::default();
    let worker = child_on("w", &up, LinkOptions::default(), &errors);
    let proc = local_type("t.proc", object(&[]), vec![("tracker", opaque("t.tracker@1").unwrap().into())]);
    let closed = log.clone();
    let proc_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let closed = closed.clone();
        let declaration = ResourceDeclaration::new(1, |_| ready(Ok(Tracker::default()))).with_close(move |_| {
            closed.push("proc closed");
            ready(Ok(()))
        });
        let tracker = decl.resource("tracker", declaration)?;
        Ok(Implementation::new().member("tracker", decl.expose(&tracker)?))
    };
    // Eager: activated before the object that holds it.
    worker.serve(&proc, proc_factory, ServeOptions::host()).unwrap();
    sleep_ms(5).await;
    let chat = local_type("t.chat10", object(&[]), vec![]);
    let (held_proc, held_upstream, logged) = (proc.clone(), upstream.clone(), log.clone());
    let chat_factory = move |_: &Json, _: &Json, decl: &mut Declare| {
        let proc = decl.get(&held_proc, json!({}), GetOptions::default())?;
        let upstream = decl.get(&held_upstream, json!({}), GetOptions::default())?;
        let (opening, logged) = (proc.clone(), logged.clone());
        let open = move |_: OpenOptions| {
            let tracker = opening.opaque_as::<Tracker>("tracker").ok().flatten();
            ready(Ok(tracker.map(|tracker| tracker.lock().unwrap().insert("chat".into())).is_some()))
        };
        let close = move |_: bool| {
            let tracker = proc.opaque_as::<Tracker>("tracker").ok().flatten();
            let state = if tracker.is_some() { "ok" } else { "gone" };
            if let Some(tracker) = &tracker {
                tracker.lock().unwrap().remove("chat");
            }
            logged.push(format!("chat closed, proc {}, tracker {state}", proc.status().as_str()));
            let call = upstream.call("note", Some(json!("bye")), None);
            async move { call.await.map(drop) }
        };
        decl.resource("session", ResourceDeclaration::new(1, open).with_close(close))?;
        Ok(Implementation::new())
    };
    worker.serve(&chat, chat_factory, ServeOptions::default()).unwrap();
    hold(&worker, &chat).ready().await.unwrap();
    worker.close().await.unwrap();
    assert_eq!(log.all(), ["chat closed, proc live, tracker ok", "server: bye", "proc closed"]);
    assert!(errors.is_empty(), "{:?}", errors.all());
}
